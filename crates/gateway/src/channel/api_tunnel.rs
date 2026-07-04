//! Gateway responder for E2E API tunnel relay legs.
//!
//! `LegClass::Api` and `LegClass::Blob` both enter here. The relay meters `Api`
//! as interactive and `Blob` as background, but after the shared Noise IK device
//! authentication the plaintext is the same HTTP-shaped tunnel protocol.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use bytes::Bytes;
use device_proto::api_tunnel::{
    self, MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest, TunnelResponse,
};
use device_proto::noise::{FrameReassembler, write_chunked};
use futures::StreamExt;
use snow::TransportState;
use tokio::io::AsyncReadExt;
use tokio_stream::wrappers::ReceiverStream;

use super::blob_service::{self, MAX_BLOB_BYTES};
use super::device_content::{
    BinarySink, BinarySource, RelayWs, TungBinSink, TungBinSource, responder_handshake,
};
use super::state::WsChannelState;

const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const TUNNEL_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const TUNNEL_UPLOAD_CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_REQUEST_IDENTITY_PREFIX: &str = "ios-device:";
const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_CONTENT_TYPE: &str = "content-type";
const HEADER_RANGE: &str = "range";
const HEADER_CONTENT_RANGE: &str = "content-range";
const JSON_MIME: &str = "application/json";

struct RequestHead {
    request_id: u64,
    method: String,
    path: String,
    headers: Vec<TunnelHeader>,
    body_len: Option<u64>,
}

pub(crate) async fn run_api_tunnel_over_relay(ws: RelayWs, state: &WsChannelState) {
    let (sink, source) = ws.split();
    if let Err(reason) = run_tunnel_session(TungBinSink(sink), TungBinSource(source), state).await {
        tracing::debug!(reason = %reason, "relay api tunnel aborted");
    }
}

async fn run_tunnel_session<Si: BinarySink, So: BinarySource>(
    mut sink: Si,
    mut source: So,
    state: &WsChannelState,
) -> Result<(), String> {
    let (mut transport, device_id) = responder_handshake(&mut sink, &mut source, state).await?;
    let mut reassembler = FrameReassembler::new();
    let mut pending = VecDeque::new();

    tracing::info!(
        device = %super::short_hash(&device_id),
        "device api tunnel established",
    );

    let first = tokio::time::timeout(
        TUNNEL_IDLE_TIMEOUT,
        next_request(&mut source, &mut transport, &mut reassembler, &mut pending),
    )
    .await
    .map_err(|_| "timed out waiting for tunnel request".to_string())??;

    let head = match first {
        Some(TunnelRequest::Head {
            request_id,
            method,
            path,
            headers,
            body_len,
        }) => RequestHead {
            request_id,
            method,
            path,
            headers,
            body_len,
        },
        Some(TunnelRequest::Cancel { reason, .. }) => {
            return Err(format!("request canceled: {reason}"));
        }
        Some(TunnelRequest::Body { request_id, .. }) => {
            send_error(
                &mut sink,
                &mut transport,
                request_id,
                400,
                "body sent before request head",
            )
            .await?;
            return Ok(());
        }
        None => return Err("peer closed before request head".into()),
    };

    if head.method.eq_ignore_ascii_case("GET")
        && let Some(blob_id) = blob_id_from_path(&head.path)
    {
        handle_blob_download(&mut sink, &mut transport, state, head, &blob_id).await?;
        return Ok(());
    }

    if head.method.eq_ignore_ascii_case("POST") && head.path == "/v1/blobs" {
        handle_blob_upload(
            &mut sink,
            &mut source,
            &mut transport,
            &mut reassembler,
            &mut pending,
            state,
            &device_id,
            head,
        )
        .await?;
        return Ok(());
    }

    send_error(
        &mut sink,
        &mut transport,
        head.request_id,
        404,
        "unsupported tunnel endpoint",
    )
    .await?;
    Ok(())
}

async fn handle_blob_download<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    state: &WsChannelState,
    head: RequestHead,
    blob_id: &str,
) -> Result<(), String> {
    let offset = blob_service::parse_range_start(header_value(&head.headers, HEADER_RANGE));
    let download =
        match blob_service::open_download(state.blob_store.as_ref(), blob_id, offset).await {
            Ok(download) => download,
            Err(err) => {
                send_service_error(sink, transport, head.request_id, err).await?;
                return Ok(());
            }
        };

    let mut headers = vec![
        TunnelHeader::new(HEADER_CONTENT_TYPE, download.mime_type),
        TunnelHeader::new(HEADER_CONTENT_LENGTH, download.body_len.to_string()),
    ];
    if let Some(content_range) = download.content_range {
        headers.push(TunnelHeader::new(HEADER_CONTENT_RANGE, content_range));
    }
    send_response(
        sink,
        transport,
        &TunnelResponse::Head {
            request_id: head.request_id,
            status: download.status.as_u16(),
            headers,
            body_len: Some(download.body_len),
        },
    )
    .await?;

    if download.body_len == 0 {
        return Ok(());
    }

    let mut reader = download.reader;
    let mut buf = vec![0u8; MAX_TUNNEL_CHUNK];
    let mut sent = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| format!("read blob: {e}"))?;
        if n == 0 {
            break;
        }
        let next_sent = sent.saturating_add(n as u64);
        send_response(
            sink,
            transport,
            &TunnelResponse::Body {
                request_id: head.request_id,
                offset: offset.saturating_add(sent),
                data: buf[..n].to_vec(),
                last: next_sent >= download.body_len,
            },
        )
        .await?;
        sent = next_sent;
        if sent >= download.body_len {
            break;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_blob_upload<Si: BinarySink, So: BinarySource>(
    sink: &mut Si,
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<TunnelRequest>,
    state: &WsChannelState,
    device_id: &str,
    head: RequestHead,
) -> Result<(), String> {
    let declared_len = match head.body_len {
        Some(len) => len,
        None => {
            send_error(
                sink,
                transport,
                head.request_id,
                411,
                "missing content length",
            )
            .await?;
            return Ok(());
        }
    };
    if declared_len > MAX_BLOB_BYTES as u64 {
        send_error(
            sink,
            transport,
            head.request_id,
            413,
            "blob exceeds size limit",
        )
        .await?;
        return Ok(());
    }
    let claimed_sha = match blob_service::require_sha256_hex(header_value(
        &head.headers,
        blob_service::HEADER_CONTENT_SHA256,
    )) {
        Ok(value) => value,
        Err(err) => {
            send_service_error(sink, transport, head.request_id, err).await?;
            return Ok(());
        }
    };
    let mime = header_value(&head.headers, HEADER_CONTENT_TYPE)
        .unwrap_or(blob_service::DEFAULT_BLOB_MIME)
        .to_owned();

    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(4);
    let stream = ReceiverStream::new(rx).boxed();
    let blob_store = state.blob_store.clone();
    let uploader = format!("{UPLOAD_REQUEST_IDENTITY_PREFIX}{device_id}");
    let put_task = tokio::spawn(async move {
        blob_service::put_upload(
            blob_store.as_ref(),
            stream,
            &mime,
            Some(&uploader),
            Some(&claimed_sha),
        )
        .await
    });

    let mut received = 0u64;
    let upload_deadline = tokio::time::Instant::now() + TUNNEL_UPLOAD_TOTAL_TIMEOUT;
    loop {
        let remaining = upload_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            abort_put_stream(tx, put_task, "upload timed out").await;
            send_error(sink, transport, head.request_id, 408, "upload timed out").await?;
            return Ok(());
        }
        let req = match tokio::time::timeout(
            std::cmp::min(remaining, TUNNEL_UPLOAD_CHUNK_IDLE_TIMEOUT),
            next_request(source, transport, reassembler, pending),
        )
        .await
        {
            Ok(Ok(Some(req))) => req,
            Ok(Ok(None)) => {
                abort_put_stream(tx, put_task, "upload leg closed").await;
                return Err("upload leg closed".into());
            }
            Ok(Err(reason)) => {
                abort_put_stream(tx, put_task, "upload decode failed").await;
                send_error(sink, transport, head.request_id, 400, &reason).await?;
                return Ok(());
            }
            Err(_) => {
                abort_put_stream(tx, put_task, "upload timed out").await;
                send_error(sink, transport, head.request_id, 408, "upload timed out").await?;
                return Ok(());
            }
        };

        match req {
            TunnelRequest::Body {
                request_id,
                offset,
                data,
                last,
            } if request_id == head.request_id => {
                if data.len() > MAX_TUNNEL_CHUNK || offset != received {
                    abort_put_stream(tx, put_task, "upload chunk offset mismatch").await;
                    send_error(
                        sink,
                        transport,
                        head.request_id,
                        400,
                        "chunk offset mismatch",
                    )
                    .await?;
                    return Ok(());
                }
                let next_received = received.saturating_add(data.len() as u64);
                if next_received > declared_len {
                    abort_put_stream(tx, put_task, "upload exceeded content length").await;
                    send_error(
                        sink,
                        transport,
                        head.request_id,
                        400,
                        "upload exceeded content length",
                    )
                    .await?;
                    return Ok(());
                }
                if last && next_received != declared_len {
                    abort_put_stream(tx, put_task, "upload ended before content length").await;
                    send_error(
                        sink,
                        transport,
                        head.request_id,
                        400,
                        "upload ended before content length",
                    )
                    .await?;
                    return Ok(());
                }
                if tx.send(Ok(Bytes::from(data))).await.is_err() {
                    break;
                }
                received = next_received;
                if last {
                    break;
                }
            }
            TunnelRequest::Cancel { reason, .. } => {
                abort_put_stream(tx, put_task, "upload canceled").await;
                send_error(sink, transport, head.request_id, 499, &reason).await?;
                return Ok(());
            }
            _ => {
                abort_put_stream(tx, put_task, "unexpected upload message").await;
                send_error(
                    sink,
                    transport,
                    head.request_id,
                    400,
                    "unexpected message during upload",
                )
                .await?;
                return Ok(());
            }
        }
    }
    drop(tx);

    let blob_ref = match put_task.await {
        Ok(Ok(blob_ref)) => blob_ref,
        Ok(Err(err)) => {
            send_service_error(sink, transport, head.request_id, err).await?;
            return Ok(());
        }
        Err(e) => return Err(format!("upload task failed: {e}")),
    };

    let body = serde_json::json!({ "blob_id": blob_ref.blob_id }).to_string();
    send_response(
        sink,
        transport,
        &TunnelResponse::Head {
            request_id: head.request_id,
            status: 201,
            headers: vec![
                TunnelHeader::new(HEADER_CONTENT_TYPE, JSON_MIME),
                TunnelHeader::new(HEADER_CONTENT_LENGTH, body.len().to_string()),
            ],
            body_len: Some(body.len() as u64),
        },
    )
    .await?;
    send_response(
        sink,
        transport,
        &TunnelResponse::Body {
            request_id: head.request_id,
            offset: 0,
            data: body.into_bytes(),
            last: true,
        },
    )
    .await?;
    Ok(())
}

async fn abort_put_stream(
    tx: tokio::sync::mpsc::Sender<io::Result<Bytes>>,
    put_task: tokio::task::JoinHandle<Result<baybo_model::BlobRef, blob_service::BlobServiceError>>,
    reason: &'static str,
) {
    let _ = tx.send(Err(io::Error::other(reason))).await;
    drop(tx);
    let _ = put_task.await;
}

async fn send_service_error<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    request_id: u64,
    err: blob_service::BlobServiceError,
) -> Result<(), String> {
    let reason = err.client_message();
    send_error(
        sink,
        transport,
        request_id,
        err.status_code().as_u16(),
        &reason,
    )
    .await
}

async fn send_error<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    request_id: u64,
    status: u16,
    reason: &str,
) -> Result<(), String> {
    send_response(
        sink,
        transport,
        &TunnelResponse::Error {
            request_id,
            status,
            reason: reason.to_owned(),
        },
    )
    .await
}

async fn send_response<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    msg: &TunnelResponse,
) -> Result<(), String> {
    let plaintext = api_tunnel::encode(msg).map_err(|e| format!("encode tunnel response: {e}"))?;
    let messages =
        write_chunked(transport, &plaintext).map_err(|e| format!("seal tunnel response: {e}"))?;
    for message in messages {
        sink.send_bytes(message)
            .await
            .map_err(|()| "send tunnel response".to_string())?;
    }
    Ok(())
}

async fn next_request<So: BinarySource>(
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<TunnelRequest>,
) -> Result<Option<TunnelRequest>, String> {
    if let Some(req) = pending.pop_front() {
        return Ok(Some(req));
    }
    loop {
        let Some(bytes) = source.next_bytes().await else {
            return Ok(None);
        };
        let frames = reassembler
            .read(transport, &bytes)
            .map_err(|e| format!("open tunnel request: {e}"))?;
        for frame in frames {
            let req = api_tunnel::decode::<TunnelRequest>(&frame)
                .map_err(|e| format!("decode tunnel request: {e}"))?;
            pending.push_back(req);
        }
        if let Some(req) = pending.pop_front() {
            return Ok(Some(req));
        }
    }
}

fn header_value<'a>(headers: &'a [TunnelHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn blob_id_from_path(path: &str) -> Option<String> {
    path.strip_prefix("/v1/blobs/")
        .filter(|blob_id| !blob_id.is_empty())
        .map(ToOwned::to_owned)
}
