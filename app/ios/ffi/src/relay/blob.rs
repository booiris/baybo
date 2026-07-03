//! The relay blob legs: dial the gateway over the relay's content-join leg with
//! the `x-relay-leg-class: blob` header (so the relay meters it as background
//! bandwidth), run the Noise IK handshake, then run the blob sub-protocol to
//! upload picked-image bytes or fetch an attachment for display.
//!
//! Like [`crate::relay::chat`], the crypto + codec live in the host-tested core
//! ([`baybo_mobile_core::blob`]); this is the WebSocket transport + file I/O.
//! Each operation is a one-shot request/response over its **own** dedicated leg,
//! so a bulk transfer never shares (or stalls) the chat leg.
//!
//! The Tauri shell's path-based `blob_download`/`blob_upload` commands (and
//! their progress channels) had no callers and were dropped in the migration;
//! only the byte-level entry points the app actually uses remain.

use std::path::Path;
use std::time::Duration;

use baybo_mobile_core::ContentHandshake;
use baybo_mobile_core::blob::{
    self, BlobDownload, BlobResponse, BlobSession, DownloadStep, MAX_BLOB_CHUNK,
};
use device_proto::noise::StaticKeypair;
use futures_util::SinkExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use super::dial::dial_content_join;
use super::pairing::{PairedRecord, load_paired_record};
use crate::transport::WsStream;

const BLOB_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// One transfer per leg, so a fixed request id is enough.
const REQUEST_ID: u64 = 1;

/// Subdir of the OS temp dir where fetched-for-display attachments are cached
/// (keyed by content hash, so re-rendering the same image never re-downloads).
const BLOB_CACHE_SUBDIR: &str = "baybo-blob-cache";

/// Subdir of the OS temp dir where a picked image is staged before its blob-leg
/// push (the picker hands us bytes, not a path). Deleted as soon as the push
/// lands. iOS may purge `tmp/` between sessions — harmless for both uses.
const BLOB_STAGING_SUBDIR: &str = "baybo-blob-staging";

/// Dial a dedicated blob leg (content-join carrying the blob class header, so the
/// relay meters it as background bandwidth and the gateway runs the blob
/// sub-protocol) and run the Noise IK handshake, returning the socket + blob
/// session.
async fn dial_blob_leg(
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<(WsStream, BlobSession), String> {
    let mut ws =
        dial_content_join(record, Some(remote_host_protocol::relay::LegClass::Blob)).await?;
    let (handshake, msg1) = ContentHandshake::start(local, &record.gateway_static_pubkey)
        .map_err(|e| format!("start handshake: {e}"))?;
    ws.send(Message::Binary(msg1))
        .await
        .map_err(|e| format!("send handshake: {e}"))?;
    let msg2 = recv_binary_with_timeout(&mut ws, BLOB_HANDSHAKE_TIMEOUT).await?;
    let session = handshake
        .finish_blob(&msg2)
        .map_err(|e| format!("finish handshake: {e}"))?;
    Ok((ws, session))
}

async fn recv_binary_with_timeout(ws: &mut WsStream, timeout: Duration) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, recv_binary(ws))
        .await
        .map_err(|_| "handshake timed out".to_string())?
}

/// Read the next binary WS message (skipping ping/pong), adapting the shared
/// [`crate::transport::recv_binary`] to this file's `String` error surface.
async fn recv_binary(ws: &mut WsStream) -> Result<Vec<u8>, String> {
    crate::transport::recv_binary(ws)
        .await
        .map_err(|e| e.to_string())
}

/// Download `blob_id` to `dest_path`. Bytes stream to a sibling `<dest_path>.part`
/// file (so a reader never sees a half-written `dest_path`, and a failed transfer
/// leaves no corrupt final file); on completion the `.part` is atomically renamed
/// onto `dest_path`. A leftover `.part` from an interrupted attempt is resumed (the
/// rename target stays untouched until the bytes are complete). Streams straight to
/// disk (no full-blob buffering).
async fn download_to_path(blob_id: &str, dest_path: &str) -> Result<(), String> {
    let expected_hex = blob::blob_id_sha256_hex(blob_id)
        .filter(|hex| is_sha256_hex(hex))
        .ok_or_else(|| "invalid blob id".to_string())?
        .to_owned();
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let (mut ws, mut session) = dial_blob_leg(&record, &local).await?;

    // Stage in a sibling `.part` (same directory ⇒ same filesystem ⇒ the final
    // rename is atomic, not a cross-device copy). Resume from its length if an
    // earlier attempt left one behind.
    let part_path = format!("{dest_path}.part");
    let mut hasher = Sha256::new();
    let resume_from = hash_existing_file_into(&part_path, &mut hasher).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
        .map_err(|e| format!("open part: {e}"))?;

    for m in session
        .seal(&blob::pull(REQUEST_ID, blob_id, resume_from))
        .map_err(|e| format!("seal pull: {e}"))?
    {
        ws.send(Message::Binary(m))
            .await
            .map_err(|e| format!("send pull: {e}"))?;
    }

    let mut download = BlobDownload::new(REQUEST_ID, blob_id, resume_from);
    loop {
        let bytes = recv_binary(&mut ws).await?;
        for resp in session.open(&bytes).map_err(|e| format!("open: {e}"))? {
            let step = match download.on_response(resp) {
                Ok(step) => step,
                Err(e) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&part_path).await;
                    let _ = ws.close(None).await;
                    return Err(e.to_string());
                }
            };
            match step {
                DownloadStep::Write { data, .. } => {
                    file.write_all(&data)
                        .await
                        .map_err(|e| format!("write part: {e}"))?;
                    hasher.update(&data);
                }
                DownloadStep::Done => {
                    let actual_hex = hex::encode(hasher.finalize());
                    if actual_hex != expected_hex {
                        drop(file);
                        let _ = tokio::fs::remove_file(&part_path).await;
                        let _ = ws.close(None).await;
                        return Err("content digest mismatch".into());
                    }
                    // Flush + close the staged file, then atomically publish it.
                    file.flush().await.map_err(|e| format!("flush part: {e}"))?;
                    drop(file);
                    tokio::fs::rename(&part_path, dest_path)
                        .await
                        .map_err(|e| format!("rename part -> dest: {e}"))?;
                    let _ = ws.close(None).await;
                    return Ok(());
                }
                DownloadStep::Idle => {}
            }
        }
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Feed an existing partial download into `hasher` so a resumed transfer still
/// verifies the final on-disk bytes, not just the suffix fetched this time.
async fn hash_existing_file_into(path: &str, hasher: &mut Sha256) -> Result<u64, String> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("open part for resume: {e}")),
    };
    let mut total = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read part for resume: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok(total)
}

/// Shared upload core: pushes the file at `src_path` as `mime_type` over a blob
/// leg and returns the minted `blob_id`. `filename_override` lets a caller declare
/// the original name when `src_path` is a staging file (defaults to the path's
/// basename).
async fn upload_from_path(
    src_path: &str,
    mime_type: &str,
    filename_override: Option<String>,
) -> Result<String, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);

    // Hash + size the file up front: the gateway content-addresses what it receives
    // and rejects a mismatch, so the phone must declare the true digest.
    let (sha256_hex, size) = hash_file(src_path).await?;
    let filename = filename_override.or_else(|| {
        Path::new(src_path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_owned())
    });

    let (mut ws, mut session) = dial_blob_leg(&record, &local).await?;

    for m in session
        .seal(&blob::push(
            REQUEST_ID,
            mime_type,
            size,
            &sha256_hex,
            filename,
        ))
        .map_err(|e| format!("seal push: {e}"))?
    {
        ws.send(Message::Binary(m))
            .await
            .map_err(|e| format!("send push: {e}"))?;
    }

    // Await the grant (and its resume offset).
    let resume_offset = loop {
        let bytes = recv_binary(&mut ws).await?;
        let mut granted = None;
        for resp in session.open(&bytes).map_err(|e| format!("open: {e}"))? {
            match resp {
                BlobResponse::PushGrant { resume_offset, .. } => granted = Some(resume_offset),
                BlobResponse::Err { reason, .. } => {
                    return Err(format!("upload refused: {reason}"));
                }
                _ => {}
            }
        }
        if let Some(offset) = granted {
            break offset;
        }
    };

    // Stream the file from the granted resume offset.
    let mut src = tokio::fs::File::open(src_path)
        .await
        .map_err(|e| format!("open src: {e}"))?;
    if resume_offset > 0 {
        src.seek(std::io::SeekFrom::Start(resume_offset))
            .await
            .map_err(|e| format!("seek src: {e}"))?;
    }
    let mut offset = resume_offset;
    let mut buf = vec![0u8; MAX_BLOB_CHUNK];
    loop {
        let n = src
            .read(&mut buf)
            .await
            .map_err(|e| format!("read src: {e}"))?;
        let last = offset + n as u64 >= size;
        for m in session
            .seal(&blob::chunk(REQUEST_ID, offset, buf[..n].to_vec(), last))
            .map_err(|e| format!("seal chunk: {e}"))?
        {
            ws.send(Message::Binary(m))
                .await
                .map_err(|e| format!("send chunk: {e}"))?;
        }
        offset += n as u64;
        if last || n == 0 {
            break;
        }
    }

    // Await the finalized id.
    loop {
        let bytes = recv_binary(&mut ws).await?;
        for resp in session.open(&bytes).map_err(|e| format!("open: {e}"))? {
            match resp {
                BlobResponse::PushDone { blob_id, .. } => {
                    let _ = ws.close(None).await;
                    return Ok(blob_id);
                }
                BlobResponse::Err { reason, .. } => return Err(format!("upload failed: {reason}")),
                _ => {}
            }
        }
    }
}

/// Upload raw `bytes` from a user-picked image (a Photos pick has no path, so
/// the bytes cross the FFI). Stages them to a content-addressed temp file,
/// uploads it over a blob leg, then removes the staging file. Returns the minted
/// `blob_id` to reference in the next message.
pub(crate) async fn upload_bytes(bytes: Vec<u8>, mime_type: String) -> Result<String, String> {
    let dir = std::env::temp_dir().join(BLOB_STAGING_SUBDIR);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create staging dir: {e}"))?;
    // Name by content hash so two concurrent picks can't collide on one path.
    let staged = dir.join(hex::encode(Sha256::digest(&bytes)));
    let staged_str = staged.to_str().ok_or("non-utf8 staging path")?.to_owned();
    tokio::fs::write(&staged, &bytes)
        .await
        .map_err(|e| format!("write staging file: {e}"))?;
    let result = upload_from_path(&staged_str, &mime_type, None).await;
    let _ = tokio::fs::remove_file(&staged).await;
    result
}

/// Fetch attachment `blob_id` for display, returning its (digest-verified) bytes.
/// Reuses the on-device cache (keyed by the blob's content hash) if present, else
/// downloads it over a blob leg and caches it.
pub(crate) async fn image_data(blob_id: String) -> Result<Vec<u8>, String> {
    let content_hex = blob::blob_id_sha256_hex(&blob_id)
        .filter(|hex| is_sha256_hex(hex))
        .ok_or_else(|| "invalid blob id".to_string())?
        .to_owned();
    let dir = std::env::temp_dir().join(BLOB_CACHE_SUBDIR);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create cache dir: {e}"))?;
    let path = dir.join(&content_hex);
    let path_str = path.to_str().ok_or("non-utf8 cache path")?.to_owned();
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        download_to_path(&blob_id, &path_str).await?;
    }
    tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read cached blob: {e}"))
}

/// SHA-256 hex + byte length of a file, streamed (no full-file buffering).
async fn hash_file(path: &str) -> Result<(String, u64), String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open src: {e}"))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read src: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}
