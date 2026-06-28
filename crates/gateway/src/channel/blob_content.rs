//! The gateway side of a relay **blob leg** — a dedicated content data leg that
//! carries the [`device_proto::blob`] sub-protocol instead of the `wire::Frame`
//! chat loop, so a bulk transfer rides its own TCP + Noise session (physically
//! isolated from chat) and the relay meters it as background bandwidth.
//!
//! The leg authenticates exactly like the chat content leg
//! ([`super::device_content::responder_handshake`] — Noise IK, device static key
//! vs an approved row) and then serves **exactly one transfer, then closes** the
//! leg (one transfer per leg — freeing its relay connection-cap slot promptly):
//!
//! - [`BlobRequest::Pull`] → stream the blob from
//!   [`BlobStore::open_at`](baybo_store::BlobStore::open_at) (token-gated) as
//!   [`BlobResponse::Chunk`]s, then [`BlobResponse::Done`].
//! - [`BlobRequest::Push`] → validate, [`BlobResponse::PushGrant`], stream the
//!   phone's [`BlobRequest::Chunk`]s into the store stamped with the device id,
//!   verify the content hash, then [`BlobResponse::PushDone`].
//! - [`BlobRequest::Abort`] → tear the leg down.
//!
//! **Read authorization is the capability token.** A `blob_id` is
//! `sha256:<hex>.<read_token>`; the phone only ever learns a token-bearing id over
//! its own authenticated chat leg, and `open_at` re-`stat`s to enforce the token —
//! so a device cannot pull a blob it was never sent.

use std::collections::VecDeque;
use std::time::Duration;

use baybo_store::{SHA256_PREFIX, StorageError};
use bytes::Bytes;
use device_proto::blob::{self, BlobRequest, BlobResponse, MAX_BLOB_CHUNK};
use device_proto::noise::{FrameReassembler, write_chunked};
use futures::StreamExt;
use snow::TransportState;
use tokio::io::AsyncReadExt;
use tokio_stream::wrappers::ReceiverStream;

use super::blobs::MAX_BLOB_BYTES;
use super::device_content::{
    BinarySink, BinarySource, RelayWs, TungBinSink, TungBinSource, responder_handshake,
};
use super::state::WsChannelState;

/// How long the leg waits for its (one) request before giving up. The gateway
/// closes the leg as soon as the transfer finishes, so this only guards the window
/// *before* the first request — a client that connects + handshakes but never asks
/// — releasing the relay connection-cap slot rather than pinning it.
const BLOB_LEG_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// After a push is granted, each next chunk/control frame must arrive within this
/// window. Without this, a client can pin the store task forever by going idle
/// after the grant.
const BLOB_UPLOAD_CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Absolute upper bound for one blob upload leg after the grant.
const BLOB_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Run the blob sub-protocol over an outbound relay blob leg (the gateway dialed
/// C's `/content/host/{relay_key}` after an `OpenDataLeg{class: Blob}` signal).
/// Blob legs are **not** deduped — they run concurrently, one per transfer,
/// bounded by the relay's per-key connection cap. Splits the leg and runs the
/// [`run_blob_session`] responder over it.
pub(crate) async fn run_blob_over_relay(ws: RelayWs, state: &WsChannelState) {
    let (sink, source) = ws.split();
    run_blob_session(TungBinSink(sink), TungBinSource(source), state).await;
}

/// The blob responder over a generic binary duplex (the relay leg in production,
/// in-memory legs in tests): authenticate the device, then serve blob requests
/// until the leg closes.
async fn run_blob_session<Si: BinarySink, So: BinarySource>(
    mut sink: Si,
    mut source: So,
    state: &WsChannelState,
) {
    let (mut transport, device_id) = match responder_handshake(&mut sink, &mut source, state).await
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!(error = %e, "relay blob leg: handshake failed");
            return;
        }
    };
    tracing::info!(device = %super::short_hash(&device_id), "blob leg established");

    let mut reassembler = FrameReassembler::new();
    let mut pending: VecDeque<BlobRequest> = VecDeque::new();
    // One transfer per leg, then close: free the relay connection-cap slot the
    // moment the transfer finishes — proactively, not waiting on the client to
    // close (it does, but a forgetful/buggy one would otherwise pin the slot until
    // the idle timeout). The idle timeout now only guards the window before the
    // first request (a client that connects + handshakes but never asks).
    match tokio::time::timeout(
        BLOB_LEG_IDLE_TIMEOUT,
        next_request(&mut source, &mut transport, &mut reassembler, &mut pending),
    )
    .await
    {
        Ok(Some(BlobRequest::Pull {
            request_id,
            blob_id,
            offset,
        })) => {
            let outcome = handle_pull(
                &mut sink,
                &mut transport,
                state,
                request_id,
                &blob_id,
                offset,
            )
            .await;
            report_outcome(&mut sink, &mut transport, request_id, outcome).await;
        }
        Ok(Some(BlobRequest::Push {
            request_id,
            mime_type,
            size,
            sha256_hex,
            ..
        })) => {
            let outcome = handle_push(
                &mut sink,
                &mut source,
                &mut transport,
                &mut reassembler,
                &mut pending,
                state,
                &device_id,
                request_id,
                &mime_type,
                size,
                &sha256_hex,
            )
            .await;
            report_outcome(&mut sink, &mut transport, request_id, outcome).await;
        }
        // A chunk outside an in-flight upload is a protocol error.
        Ok(Some(BlobRequest::Chunk { request_id, .. })) => {
            report_outcome(
                &mut sink,
                &mut transport,
                request_id,
                Err(XferError::Protocol("unexpected chunk".into())),
            )
            .await;
        }
        // The client aborted, or closed before sending a request.
        Ok(Some(BlobRequest::Abort { .. })) | Ok(None) => {}
        // Idle before any request (connected + handshaked but never asked).
        Err(_) => {
            tracing::debug!(device = %super::short_hash(&device_id), "blob leg idle; closing");
        }
    }
    sink.close().await;
    tracing::info!(device = %super::short_hash(&device_id), "blob leg closed");
}

/// Why a transfer handler ended: a protocol failure the leg survives (reported to
/// the phone as [`BlobResponse::Err`]), or a dead leg (tear it down).
enum XferError {
    /// Reportable to the phone via [`BlobResponse::Err`] before the leg closes.
    Protocol(String),
    /// The transport/socket is already gone; nothing to report.
    Leg,
}

type PutTask = tokio::task::JoinHandle<Result<baybo_model::BlobRef, StorageError>>;

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

async fn abort_put_stream(
    tx: tokio::sync::mpsc::Sender<std::io::Result<Bytes>>,
    put_task: PutTask,
    reason: &'static str,
) {
    let _ = tx
        .send(Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            reason,
        )))
        .await;
    drop(tx);
    let _ = put_task.await;
}

/// Report a transfer outcome to the phone before the leg closes: `Ok` is a no-op,
/// a `Protocol` error sends a best-effort [`BlobResponse::Err`]. A `Leg` error
/// means the socket is already gone, so there is nothing to send — the caller
/// closes the leg in every case either way.
async fn report_outcome<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    request_id: u64,
    outcome: Result<(), XferError>,
) {
    if let Err(XferError::Protocol(reason)) = outcome {
        let _ = send_response(sink, transport, &BlobResponse::Err { request_id, reason }).await;
    }
}

/// Stream `blob_id` from `offset` as [`BlobResponse::Chunk`]s then a final
/// [`BlobResponse::Done`]. The token is enforced by `open_at`/`stat`.
async fn handle_pull<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    state: &WsChannelState,
    request_id: u64,
    blob_id: &str,
    offset: u64,
) -> Result<(), XferError> {
    // `stat` enforces the read_token and gives the full size for the Done marker.
    let meta = match state.blob_store.stat(blob_id).await {
        Ok(m) => m,
        Err(StorageError::NotFound(_)) => {
            return Err(XferError::Protocol("unknown or unauthorized blob".into()));
        }
        Err(e) => return Err(XferError::Protocol(format!("stat blob: {e}"))),
    };
    let total_size = meta.size;
    // `open_at` re-`stat`s (token gate) before seeking — never resolves by hex.
    let mut reader = match state.blob_store.open_at(blob_id, offset).await {
        Ok(r) => r,
        Err(StorageError::NotFound(_)) => {
            return Err(XferError::Protocol("unknown or unauthorized blob".into()));
        }
        Err(e) => return Err(XferError::Protocol(format!("open blob: {e}"))),
    };

    // The content digest (hex prefix of the id) so the phone can verify integrity.
    let sha256_hex = blob_id
        .strip_prefix(SHA256_PREFIX)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or_default()
        .to_string();

    let mut buf = vec![0u8; MAX_BLOB_CHUNK];
    let mut sent = offset;
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| XferError::Protocol(format!("read blob: {e}")))?;
        if n == 0 {
            break;
        }
        let chunk_offset = sent;
        sent += n as u64;
        let last = sent >= total_size;
        send_response(
            sink,
            transport,
            &BlobResponse::Chunk {
                request_id,
                offset: chunk_offset,
                data: buf[..n].to_vec(),
                last,
            },
        )
        .await
        .map_err(|()| XferError::Leg)?;
        if last {
            break;
        }
    }
    send_response(
        sink,
        transport,
        &BlobResponse::Done {
            request_id,
            total_size,
            sha256_hex,
        },
    )
    .await
    .map_err(|()| XferError::Leg)?;
    Ok(())
}

/// Receive an upload: grant, stream the phone's [`BlobRequest::Chunk`]s
/// into [`BlobStore::put_stream`](baybo_store::BlobStore::put_stream) (stamping the
/// `device_id` as uploader), verify the finalized content hash matches the claim,
/// and reply [`BlobResponse::PushDone`]. The Noise handshake already proved the
/// device is approved (the upload gate); [`MAX_BLOB_BYTES`] bounds one upload.
#[allow(clippy::too_many_arguments)]
async fn handle_push<Si: BinarySink, So: BinarySource>(
    sink: &mut Si,
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<BlobRequest>,
    state: &WsChannelState,
    device_id: &str,
    request_id: u64,
    mime_type: &str,
    size: u64,
    sha256_hex: &str,
) -> Result<(), XferError> {
    if size > MAX_BLOB_BYTES as u64 {
        return Err(XferError::Protocol("blob exceeds the size limit".into()));
    }
    if !is_sha256_hex(sha256_hex) {
        return Err(XferError::Protocol("invalid content hash".into()));
    }

    // Authorized; the phone (re)starts at offset 0 (same-session resume of an
    // interrupted upload is a follow-up — a fresh request_id restarts).
    send_response(
        sink,
        transport,
        &BlobResponse::PushGrant {
            request_id,
            resume_offset: 0,
        },
    )
    .await
    .map_err(|()| XferError::Leg)?;

    // Stream incoming chunks into put_stream over a bounded channel (so a slow disk
    // backpressures the leg rather than buffering the whole blob in memory).
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
    let stream = ReceiverStream::new(rx).boxed();
    let blob_store = state.blob_store.clone();
    let mime = mime_type.to_string();
    let uploader = device_id.to_string();
    let put_task = tokio::spawn(async move {
        blob_store
            .put_stream(stream, &mime, Some(&uploader), size)
            .await
    });

    let mut received: u64 = 0;
    let upload_deadline = tokio::time::Instant::now() + BLOB_UPLOAD_TOTAL_TIMEOUT;
    loop {
        let now = tokio::time::Instant::now();
        let remaining = upload_deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            abort_put_stream(tx, put_task, "upload timed out").await;
            return Err(XferError::Protocol("upload timed out".into()));
        }
        let wait_for = std::cmp::min(remaining, BLOB_UPLOAD_CHUNK_IDLE_TIMEOUT);
        let req = match tokio::time::timeout(
            wait_for,
            next_request(source, transport, reassembler, pending),
        )
        .await
        {
            Ok(Some(r)) => r,
            Ok(None) => {
                abort_put_stream(tx, put_task, "upload leg closed").await;
                return Err(XferError::Leg);
            }
            Err(_) => {
                abort_put_stream(tx, put_task, "upload timed out").await;
                return Err(XferError::Protocol("upload timed out".into()));
            }
        };
        match req {
            BlobRequest::Chunk {
                request_id: rid,
                offset,
                data,
                last,
            } if rid == request_id => {
                if offset != received {
                    abort_put_stream(tx, put_task, "upload chunk offset mismatch").await;
                    return Err(XferError::Protocol(format!(
                        "chunk offset {offset} != expected {received}",
                    )));
                }
                let next_received = received.saturating_add(data.len() as u64);
                if next_received > size {
                    abort_put_stream(tx, put_task, "upload exceeded declared size").await;
                    return Err(XferError::Protocol(
                        "upload exceeded its declared size".into(),
                    ));
                }
                if next_received == size && !last {
                    abort_put_stream(tx, put_task, "upload missing final marker").await;
                    return Err(XferError::Protocol("upload missing final marker".into()));
                }
                if last && next_received != size {
                    abort_put_stream(tx, put_task, "upload ended before declared size").await;
                    return Err(XferError::Protocol(
                        "upload ended before its declared size".into(),
                    ));
                }
                // If the receiver is gone the put task already failed; stop feeding.
                if tx.send(Ok(Bytes::from(data))).await.is_err() {
                    break;
                }
                received = next_received;
                if last {
                    break;
                }
            }
            BlobRequest::Abort { .. } => {
                abort_put_stream(tx, put_task, "upload aborted").await;
                return Err(XferError::Protocol("upload aborted".into()));
            }
            _ => {
                abort_put_stream(tx, put_task, "unexpected upload message").await;
                return Err(XferError::Protocol(
                    "unexpected message during upload".into(),
                ));
            }
        }
    }
    drop(tx); // close the stream so put_stream finalizes

    let blob_ref = match put_task.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(XferError::Protocol(format!("store upload: {e}"))),
        Err(_) => return Err(XferError::Leg),
    };

    // Integrity: the content-addressed hex must match what the phone claimed; if
    // not, drop the blob so a lie can't leave a durable mislabeled object.
    let got_hex = blob_ref
        .blob_id
        .strip_prefix(SHA256_PREFIX)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or_default();
    if got_hex != sha256_hex {
        let _ = state.blob_store.delete(&blob_ref.blob_id).await;
        return Err(XferError::Protocol("content hash mismatch".into()));
    }

    send_response(
        sink,
        transport,
        &BlobResponse::PushDone {
            request_id,
            blob_id: blob_ref.blob_id,
        },
    )
    .await
    .map_err(|()| XferError::Leg)?;
    Ok(())
}

/// Encode + Noise-seal one [`BlobResponse`] and send its chunk(s). `Err(())` means
/// the leg is gone (a send failure), so the caller tears down.
async fn send_response<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    msg: &BlobResponse,
) -> Result<(), ()> {
    let plaintext = match blob::encode(msg) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "encode blob response failed");
            return Ok(()); // skip a bad message rather than kill the leg
        }
    };
    let messages = match write_chunked(transport, &plaintext) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "blob leg noise encrypt failed");
            return Err(());
        }
    };
    for message in messages {
        sink.send_bytes(message).await?;
    }
    Ok(())
}

/// Read the next decoded [`BlobRequest`], reassembling + decrypting Noise messages.
/// `None` on close or an unrecoverable transport error.
async fn next_request<R: BinarySource>(
    source: &mut R,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<BlobRequest>,
) -> Option<BlobRequest> {
    loop {
        if let Some(req) = pending.pop_front() {
            return Some(req);
        }
        let bytes = source.next_bytes().await?;
        let reassembled = match reassembler.read(transport, &bytes) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!(error = %e, "blob leg decrypt failed; tearing down");
                return None;
            }
        };
        for msg_bytes in reassembled {
            match blob::decode::<BlobRequest>(&msg_bytes) {
                Ok(req) => pending.push_back(req),
                Err(e) => tracing::warn!(error = %e, "decode blob request failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::load_or_create_static_keypair;
    use crate::test_support::build_test_deps;
    use baybo_store::{DeviceRow, DeviceStatus};
    use device_proto::noise::{NOISE_MAX_MESSAGE, StaticKeypair};
    use snow::HandshakeState;

    fn device_row(device_id: &str, pubkey: Vec<u8>) -> DeviceRow {
        DeviceRow {
            device_id: device_id.into(),
            device_pubkey: pubkey,
            auth_token: "device-auth-token-fixed-0123456789abcdef".into(),
            status: DeviceStatus::Approved,
            rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
            created_at: 0,
            approved_at: Some(0),
            last_seen_at: None,
            relay_url: "wss://relay.test".into(),
            remote_api_key: "inst-test".into(),
        }
    }

    // In-memory binary legs standing in for a spliced relay blob leg.
    struct ChanSink(tokio::sync::mpsc::Sender<Vec<u8>>);
    struct ChanSource(tokio::sync::mpsc::Receiver<Vec<u8>>);

    #[async_trait::async_trait]
    impl BinarySink for ChanSink {
        async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
            self.0.send(bytes).await.map_err(|_| ())
        }
        async fn close(&mut self) {}
    }

    #[async_trait::async_trait]
    impl BinarySource for ChanSource {
        async fn next_bytes(&mut self) -> Option<Vec<u8>> {
            self.0.recv().await
        }
    }

    /// A download over a blob leg: an approved device pulls a blob it holds the
    /// (token-bearing) id for; the gateway streams it as Noise-sealed chunks across
    /// the Noise per-message boundary, ending with `Done` carrying the digest.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pull_streams_a_blob_in_chunks_then_done() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        tg.deps
            .stores
            .device
            .create(&device_row("ios-dev", device.public().to_vec()))
            .await
            .expect("seed approved device row");
        // Stage a blob larger than one chunk so the chunk loop + reassembly run.
        let payload: Vec<u8> = (0..(MAX_BLOB_CHUNK * 2 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();
        let blob_ref = tg
            .deps
            .stores
            .blob
            .put(&payload, "application/octet-stream", None)
            .await
            .expect("stage blob");

        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .expect("gateway static key");
        let gw_pub = gw_static.public();
        let state = WsChannelState::from_deps(&tg.deps);

        let (gw_tx, mut phone_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (phone_tx, gw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move {
            run_blob_session(ChanSink(gw_tx), ChanSource(gw_rx), &state).await;
        });

        // Phone IK initiator handshake.
        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        phone_tx.send(buf[..n].to_vec()).await.unwrap();
        let msg2 = phone_rx.recv().await.expect("gateway msg2");
        hs.read_message(&msg2, &mut buf).unwrap();
        let mut transport = hs.into_transport_mode().unwrap();

        // Pull from offset 0.
        let pull = BlobRequest::Pull {
            request_id: 1,
            blob_id: blob_ref.blob_id.clone(),
            offset: 0,
        };
        for msg in write_chunked(&mut transport, &blob::encode(&pull).unwrap()).unwrap() {
            phone_tx.send(msg).await.unwrap();
        }

        // Collect chunks until Done; reassemble the payload.
        let mut got = Vec::new();
        let mut reasm = FrameReassembler::new();
        let mut done_digest = None;
        'outer: for _ in 0..256 {
            let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), phone_rx.recv())
                .await
                .expect("chunk within timeout")
                .expect("leg open");
            for logical in reasm.read(&mut transport, &bytes).unwrap() {
                match blob::decode::<BlobResponse>(&logical).unwrap() {
                    BlobResponse::Chunk { offset, data, .. } => {
                        assert_eq!(offset as usize, got.len(), "chunks arrive in order");
                        got.extend_from_slice(&data);
                    }
                    BlobResponse::Done {
                        total_size,
                        sha256_hex,
                        ..
                    } => {
                        assert_eq!(total_size as usize, payload.len());
                        done_digest = Some(sha256_hex);
                        break 'outer;
                    }
                    other => panic!("unexpected blob response: {other:?}"),
                }
            }
        }
        assert_eq!(got, payload, "the reassembled blob matches the original");
        let expected_hex = blob_ref
            .blob_id
            .strip_prefix(SHA256_PREFIX)
            .and_then(|r| r.split('.').next())
            .unwrap();
        assert_eq!(
            done_digest.as_deref(),
            Some(expected_hex),
            "Done carries the content digest the phone verifies against the id"
        );
    }

    /// An upload over a blob leg: the phone offers a blob, the gateway grants,
    /// streams the chunks into the store stamped with the device id, verifies the
    /// content hash, and returns the content-addressed id — and the bytes land.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_streams_chunks_into_the_store_and_returns_the_id() {
        use sha2::{Digest, Sha256};

        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        tg.deps
            .stores
            .device
            .create(&device_row("ios-dev", device.public().to_vec()))
            .await
            .expect("seed approved device row");
        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .unwrap();
        let gw_pub = gw_static.public();
        let blob_store = tg.deps.stores.blob.clone();
        let state = WsChannelState::from_deps(&tg.deps);

        let (gw_tx, mut phone_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (phone_tx, gw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move {
            run_blob_session(ChanSink(gw_tx), ChanSource(gw_rx), &state).await;
        });

        // Handshake.
        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        phone_tx.send(buf[..n].to_vec()).await.unwrap();
        let msg2 = phone_rx.recv().await.expect("gateway msg2");
        hs.read_message(&msg2, &mut buf).unwrap();
        let mut transport = hs.into_transport_mode().unwrap();

        // A payload spanning >1 chunk; claim its true digest.
        let payload: Vec<u8> = (0..(MAX_BLOB_CHUNK + 777))
            .map(|i| (i % 251) as u8)
            .collect();
        let sha256_hex = hex::encode(Sha256::digest(&payload));

        // Offer the upload.
        let push = BlobRequest::Push {
            request_id: 5,
            mime_type: "application/octet-stream".into(),
            size: payload.len() as u64,
            sha256_hex: sha256_hex.clone(),
            filename: Some("f.bin".into()),
        };
        for m in write_chunked(&mut transport, &blob::encode(&push).unwrap()).unwrap() {
            phone_tx.send(m).await.unwrap();
        }

        // Expect a grant.
        let grant = recv_response(&mut phone_rx, &mut transport).await;
        assert!(
            matches!(
                grant,
                BlobResponse::PushGrant {
                    request_id: 5,
                    resume_offset: 0
                }
            ),
            "expected PushGrant, got {grant:?}"
        );

        // Send the chunks.
        for (i, chunk) in payload.chunks(MAX_BLOB_CHUNK).enumerate() {
            let last = (i + 1) * MAX_BLOB_CHUNK >= payload.len();
            let msg = BlobRequest::Chunk {
                request_id: 5,
                offset: (i * MAX_BLOB_CHUNK) as u64,
                data: chunk.to_vec(),
                last,
            };
            for m in write_chunked(&mut transport, &blob::encode(&msg).unwrap()).unwrap() {
                phone_tx.send(m).await.unwrap();
            }
        }

        // Expect PushDone with the content-addressed id, and the bytes are stored.
        let done = recv_response(&mut phone_rx, &mut transport).await;
        let blob_id = match done {
            BlobResponse::PushDone {
                request_id: 5,
                blob_id,
            } => blob_id,
            other => panic!("expected PushDone, got {other:?}"),
        };
        assert!(blob_id.starts_with(&format!("{SHA256_PREFIX}{sha256_hex}")));
        let stored = blob_store.get(&blob_id).await.expect("blob landed");
        assert_eq!(stored, payload, "uploaded bytes match");
    }

    /// An upload whose claimed hash doesn't match the streamed bytes is rejected
    /// and the mislabeled blob is not left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_with_a_mismatched_hash_is_rejected() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        tg.deps
            .stores
            .device
            .create(&device_row("ios-dev", device.public().to_vec()))
            .await
            .expect("seed approved device row");
        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .unwrap();
        let gw_pub = gw_static.public();
        let state = WsChannelState::from_deps(&tg.deps);

        let (gw_tx, mut phone_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (phone_tx, gw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move {
            run_blob_session(ChanSink(gw_tx), ChanSource(gw_rx), &state).await;
        });
        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        phone_tx.send(buf[..n].to_vec()).await.unwrap();
        let msg2 = phone_rx.recv().await.expect("gateway msg2");
        hs.read_message(&msg2, &mut buf).unwrap();
        let mut transport = hs.into_transport_mode().unwrap();

        let body = b"some bytes".to_vec();
        let push = BlobRequest::Push {
            request_id: 7,
            mime_type: "text/plain".into(),
            size: body.len() as u64,
            sha256_hex: "00".repeat(32), // a lie
            filename: None,
        };
        for m in write_chunked(&mut transport, &blob::encode(&push).unwrap()).unwrap() {
            phone_tx.send(m).await.unwrap();
        }
        let _grant = recv_response(&mut phone_rx, &mut transport).await;
        let chunk = BlobRequest::Chunk {
            request_id: 7,
            offset: 0,
            data: body,
            last: true,
        };
        for m in write_chunked(&mut transport, &blob::encode(&chunk).unwrap()).unwrap() {
            phone_tx.send(m).await.unwrap();
        }
        let resp = recv_response(&mut phone_rx, &mut transport).await;
        assert!(
            matches!(resp, BlobResponse::Err { request_id: 7, .. }),
            "a hash mismatch is rejected, got {resp:?}"
        );
    }

    /// Read one decoded [`BlobResponse`] from the phone side of the in-memory leg.
    async fn recv_response(
        rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
        transport: &mut TransportState,
    ) -> BlobResponse {
        let mut reasm = FrameReassembler::new();
        loop {
            let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("response within timeout")
                .expect("leg open");
            let mut msgs = reasm.read(transport, &bytes).unwrap();
            if let Some(first) = msgs.drain(..).next() {
                return blob::decode::<BlobResponse>(&first).unwrap();
            }
        }
    }

    /// A pull for an id whose token does not match (a bare-hex / wrong-token probe)
    /// is refused — the token capability gate holds on the blob leg.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pull_with_a_wrong_token_is_refused() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        tg.deps
            .stores
            .device
            .create(&device_row("ios-dev", device.public().to_vec()))
            .await
            .expect("seed approved device row");
        let blob_ref = tg
            .deps
            .stores
            .blob
            .put(b"secret", "text/plain", None)
            .await
            .expect("stage blob");
        // Strip the token, keep the hex — a device that learned only the digest.
        let hex_only = blob_ref.blob_id.split('.').next().unwrap().to_string() + ".not-the-token";

        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .unwrap();
        let gw_pub = gw_static.public();
        let state = WsChannelState::from_deps(&tg.deps);

        let (gw_tx, mut phone_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let (phone_tx, gw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            run_blob_session(ChanSink(gw_tx), ChanSource(gw_rx), &state).await;
        });

        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        phone_tx.send(buf[..n].to_vec()).await.unwrap();
        let msg2 = phone_rx.recv().await.expect("gateway msg2");
        hs.read_message(&msg2, &mut buf).unwrap();
        let mut transport = hs.into_transport_mode().unwrap();

        let pull = BlobRequest::Pull {
            request_id: 9,
            blob_id: hex_only,
            offset: 0,
        };
        for msg in write_chunked(&mut transport, &blob::encode(&pull).unwrap()).unwrap() {
            phone_tx.send(msg).await.unwrap();
        }

        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), phone_rx.recv())
            .await
            .expect("response within timeout")
            .expect("leg open");
        let mut reasm = FrameReassembler::new();
        let logical = reasm.read(&mut transport, &bytes).unwrap().remove(0);
        match blob::decode::<BlobResponse>(&logical).unwrap() {
            BlobResponse::Err { request_id, .. } => assert_eq!(request_id, 9),
            other => panic!("expected Err for a wrong-token pull, got {other:?}"),
        }
    }

    #[test]
    fn sha256_hex_validation_requires_lowercase_64_hex_chars() {
        let mut non_hex = "0".repeat(63);
        non_hex.push('z');
        assert!(is_sha256_hex(&"ab".repeat(32)));
        assert!(!is_sha256_hex(&"AB".repeat(32)));
        assert!(!is_sha256_hex("abc"));
        assert!(!is_sha256_hex(&non_hex));
    }

    /// A chunk whose offset does not match the bytes already accepted is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_with_a_bad_chunk_offset_is_rejected() {
        use sha2::{Digest, Sha256};

        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        tg.deps
            .stores
            .device
            .create(&device_row("ios-dev", device.public().to_vec()))
            .await
            .expect("seed approved device row");
        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .unwrap();
        let gw_pub = gw_static.public();
        let state = WsChannelState::from_deps(&tg.deps);

        let (gw_tx, mut phone_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (phone_tx, gw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move {
            run_blob_session(ChanSink(gw_tx), ChanSource(gw_rx), &state).await;
        });
        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        phone_tx.send(buf[..n].to_vec()).await.unwrap();
        let msg2 = phone_rx.recv().await.expect("gateway msg2");
        hs.read_message(&msg2, &mut buf).unwrap();
        let mut transport = hs.into_transport_mode().unwrap();

        let body = b"offset checked".to_vec();
        let push = BlobRequest::Push {
            request_id: 11,
            mime_type: "text/plain".into(),
            size: body.len() as u64,
            sha256_hex: hex::encode(Sha256::digest(&body)),
            filename: None,
        };
        for m in write_chunked(&mut transport, &blob::encode(&push).unwrap()).unwrap() {
            phone_tx.send(m).await.unwrap();
        }
        let grant = recv_response(&mut phone_rx, &mut transport).await;
        assert!(matches!(
            grant,
            BlobResponse::PushGrant { request_id: 11, .. }
        ));

        let chunk = BlobRequest::Chunk {
            request_id: 11,
            offset: 1,
            data: body,
            last: true,
        };
        for m in write_chunked(&mut transport, &blob::encode(&chunk).unwrap()).unwrap() {
            phone_tx.send(m).await.unwrap();
        }
        let resp = recv_response(&mut phone_rx, &mut transport).await;
        assert!(
            matches!(resp, BlobResponse::Err { request_id: 11, .. }),
            "a bad offset is rejected, got {resp:?}"
        );
    }
}
