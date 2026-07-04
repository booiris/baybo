//! Relay blob transfer over the API tunnel.
//!
//! Each operation uses its own `x-relay-leg-class: blob` data leg, preserving the
//! background bandwidth class and physical isolation from chat. The bytes now
//! ride URL-shaped tunnel messages (`GET/POST /v1/blobs`) instead of the
//! previous bespoke blob protocol.

use std::time::{Duration, Instant};

use baybo_mobile_core::{
    ApiTunnelSession, ContentHandshake, MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest,
    TunnelResponse, blob_id_sha256_hex,
};
use device_proto::noise::StaticKeypair;
use futures_util::SinkExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::ipc::Channel;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use super::dial::dial_content_join;
use super::pairing::{PairedRecord, load_paired_record};
use crate::transport::WsStream;

const BLOB_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_ID: u64 = 1;
const BLOB_CACHE_SUBDIR: &str = "baybo-blob-cache";
const BLOB_STAGING_SUBDIR: &str = "baybo-blob-staging";
const HEADER_CONTENT_TYPE: &str = "content-type";
const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_CONTENT_SHA256: &str = "x-baybo-content-sha256";
const HEADER_RANGE: &str = "range";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

struct ProgressThrottle {
    channel: Channel<u64>,
    last: Option<Instant>,
}

impl ProgressThrottle {
    fn new(channel: Channel<u64>) -> Self {
        Self {
            channel,
            last: None,
        }
    }

    fn update(&mut self, bytes: u64) {
        let now = Instant::now();
        if self
            .last
            .is_none_or(|t| now.duration_since(t) >= PROGRESS_INTERVAL)
        {
            let _ = self.channel.send(bytes);
            self.last = Some(now);
        }
    }

    fn finish(&self, bytes: u64) {
        let _ = self.channel.send(bytes);
    }
}

async fn dial_blob_leg(
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<(WsStream, ApiTunnelSession), String> {
    let mut ws =
        dial_content_join(record, Some(remote_host_protocol::relay::LegClass::Blob)).await?;
    let (handshake, msg1) = ContentHandshake::start(local, &record.gateway_static_pubkey)
        .map_err(|e| format!("start handshake: {e}"))?;
    ws.send(Message::Binary(msg1))
        .await
        .map_err(|e| format!("send handshake: {e}"))?;
    let msg2 = recv_binary_with_timeout(&mut ws, BLOB_HANDSHAKE_TIMEOUT).await?;
    let session = handshake
        .finish_api_tunnel(&msg2)
        .map_err(|e| format!("finish handshake: {e}"))?;
    Ok((ws, session))
}

async fn recv_binary_with_timeout(ws: &mut WsStream, timeout: Duration) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, recv_binary(ws))
        .await
        .map_err(|_| "handshake timed out".to_string())?
}

async fn recv_binary(ws: &mut WsStream) -> Result<Vec<u8>, String> {
    crate::transport::recv_binary(ws)
        .await
        .map_err(|e| e.to_string())
}

async fn send_request(
    ws: &mut WsStream,
    session: &mut ApiTunnelSession,
    request: &TunnelRequest,
) -> Result<(), String> {
    for message in session
        .seal(request)
        .map_err(|e| format!("seal tunnel request: {e}"))?
    {
        ws.send(Message::Binary(message))
            .await
            .map_err(|e| format!("send tunnel request: {e}"))?;
    }
    Ok(())
}

async fn next_response(
    ws: &mut WsStream,
    session: &mut ApiTunnelSession,
) -> Result<TunnelResponse, String> {
    loop {
        let bytes = recv_binary(ws).await?;
        let responses = session
            .open(&bytes)
            .map_err(|e| format!("open tunnel response: {e}"))?;
        if let Some(response) = responses.into_iter().next() {
            return Ok(response);
        }
    }
}

async fn expect_response_head(
    ws: &mut WsStream,
    session: &mut ApiTunnelSession,
) -> Result<(u16, Vec<TunnelHeader>, Option<u64>), String> {
    loop {
        match next_response(ws, session).await? {
            TunnelResponse::Head {
                status,
                headers,
                body_len,
                ..
            } => return Ok((status, headers, body_len)),
            TunnelResponse::Error { status, reason, .. } => {
                return Err(format!("HTTP {status}: {reason}"));
            }
            TunnelResponse::Body { .. } => {}
        }
    }
}

pub async fn download(
    blob_id: String,
    dest_path: String,
    on_progress: Channel<u64>,
) -> Result<(), String> {
    download_to_path(
        &blob_id,
        &dest_path,
        Some(ProgressThrottle::new(on_progress)),
    )
    .await
}

async fn download_to_path(
    blob_id: &str,
    dest_path: &str,
    mut progress: Option<ProgressThrottle>,
) -> Result<(), String> {
    let expected_hex = blob_id_sha256_hex(blob_id)
        .filter(|hex| is_sha256_hex(hex))
        .ok_or_else(|| "invalid blob id".to_string())?
        .to_owned();
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let (mut ws, mut session) = dial_blob_leg(&record, &local).await?;

    let part_path = format!("{dest_path}.part");
    let mut hasher = Sha256::new();
    let resume_from = hash_existing_file_into(&part_path, &mut hasher).await?;
    let mut headers = Vec::new();
    if resume_from > 0 {
        headers.push(TunnelHeader::new(
            HEADER_RANGE,
            format!("bytes={resume_from}-"),
        ));
    }
    send_request(
        &mut ws,
        &mut session,
        &TunnelRequest::Head {
            request_id: REQUEST_ID,
            method: "GET".into(),
            path: format!("/v1/blobs/{blob_id}"),
            headers,
            body_len: None,
        },
    )
    .await?;

    let (status, _headers, body_len) = expect_response_head(&mut ws, &mut session).await?;
    if !(status == 200 || status == 206) {
        return Err(format!("download failed: HTTP {status}"));
    }
    if resume_from > 0 && status != 206 {
        return Err("download resume was not accepted".into());
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
        .map_err(|e| format!("open part: {e}"))?;

    let mut expected_body_offset = resume_from;
    if body_len == Some(0) {
        finalize_download(file, &part_path, dest_path, hasher, &expected_hex).await?;
        if let Some(p) = progress.as_ref() {
            p.finish(resume_from);
        }
        let _ = ws.close(None).await;
        return Ok(());
    }

    loop {
        match next_response(&mut ws, &mut session).await? {
            TunnelResponse::Body {
                offset,
                data,
                last,
                ..
            } => {
                if offset != expected_body_offset {
                    drop(file);
                    let _ = tokio::fs::remove_file(&part_path).await;
                    let _ = ws.close(None).await;
                    return Err(format!(
                        "body offset {offset} != expected {expected_body_offset}",
                    ));
                }
                file.write_all(&data)
                    .await
                    .map_err(|e| format!("write part: {e}"))?;
                hasher.update(&data);
                expected_body_offset += data.len() as u64;
                if let Some(p) = progress.as_mut() {
                    p.update(expected_body_offset);
                }
                if last {
                    finalize_download(file, &part_path, dest_path, hasher, &expected_hex).await?;
                    if let Some(p) = progress.as_ref() {
                        p.finish(expected_body_offset);
                    }
                    let _ = ws.close(None).await;
                    return Ok(());
                }
            }
            TunnelResponse::Error { status, reason, .. } => {
                return Err(format!("download failed: HTTP {status}: {reason}"));
            }
            TunnelResponse::Head { .. } => {}
        }
    }
}

async fn finalize_download(
    mut file: tokio::fs::File,
    part_path: &str,
    dest_path: &str,
    hasher: Sha256,
    expected_hex: &str,
) -> Result<(), String> {
    let actual_hex = hex::encode(hasher.finalize());
    if actual_hex != expected_hex {
        drop(file);
        let _ = tokio::fs::remove_file(part_path).await;
        return Err("content digest mismatch".into());
    }
    file.flush().await.map_err(|e| format!("flush part: {e}"))?;
    drop(file);
    tokio::fs::rename(part_path, dest_path)
        .await
        .map_err(|e| format!("rename part -> dest: {e}"))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

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

pub async fn upload(
    src_path: String,
    mime_type: String,
    on_progress: Channel<u64>,
) -> Result<String, String> {
    upload_from_path(
        &src_path,
        &mime_type,
        Some(ProgressThrottle::new(on_progress)),
    )
    .await
}

async fn upload_from_path(
    src_path: &str,
    mime_type: &str,
    mut progress: Option<ProgressThrottle>,
) -> Result<String, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let (sha256_hex, size) = hash_file(src_path).await?;
    let (mut ws, mut session) = dial_blob_leg(&record, &local).await?;

    send_request(
        &mut ws,
        &mut session,
        &TunnelRequest::Head {
            request_id: REQUEST_ID,
            method: "POST".into(),
            path: "/v1/blobs".into(),
            headers: vec![
                TunnelHeader::new(HEADER_CONTENT_TYPE, mime_type),
                TunnelHeader::new(HEADER_CONTENT_LENGTH, size.to_string()),
                TunnelHeader::new(HEADER_CONTENT_SHA256, sha256_hex),
            ],
            body_len: Some(size),
        },
    )
    .await?;

    let mut src = tokio::fs::File::open(src_path)
        .await
        .map_err(|e| format!("open src: {e}"))?;
    let mut offset = 0u64;
    let mut buf = vec![0u8; MAX_TUNNEL_CHUNK];
    if size == 0 {
        send_request(
            &mut ws,
            &mut session,
            &TunnelRequest::Body {
                request_id: REQUEST_ID,
                offset: 0,
                data: Vec::new(),
                last: true,
            },
        )
        .await?;
    } else {
        loop {
            let n = src
                .read(&mut buf)
                .await
                .map_err(|e| format!("read src: {e}"))?;
            if n == 0 {
                break;
            }
            let last = offset + n as u64 >= size;
            send_request(
                &mut ws,
                &mut session,
                &TunnelRequest::Body {
                    request_id: REQUEST_ID,
                    offset,
                    data: buf[..n].to_vec(),
                    last,
                },
            )
            .await?;
            offset += n as u64;
            if let Some(p) = progress.as_mut() {
                p.update(offset);
            }
            if last {
                break;
            }
        }
    }
    if let Some(p) = progress.as_ref() {
        p.finish(offset);
    }

    let (status, _headers, _body_len) = expect_response_head(&mut ws, &mut session).await?;
    if !(200..300).contains(&status) {
        return Err(format!("upload failed: HTTP {status}"));
    }
    let mut body = Vec::new();
    loop {
        match next_response(&mut ws, &mut session).await? {
            TunnelResponse::Body { data, last, .. } => {
                body.extend(data);
                if last {
                    break;
                }
            }
            TunnelResponse::Error { status, reason, .. } => {
                return Err(format!("upload failed: HTTP {status}: {reason}"));
            }
            TunnelResponse::Head { .. } => {}
        }
    }
    let _ = ws.close(None).await;
    #[derive(Deserialize)]
    struct BlobIdResp {
        blob_id: String,
    }
    let parsed: BlobIdResp =
        serde_json::from_slice(&body).map_err(|e| format!("decode blob id: {e}"))?;
    Ok(parsed.blob_id)
}

pub async fn upload_bytes(bytes: Vec<u8>, mime_type: String) -> Result<String, String> {
    let dir = std::env::temp_dir().join(BLOB_STAGING_SUBDIR);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create staging dir: {e}"))?;
    let staged = dir.join(hex::encode(Sha256::digest(&bytes)));
    let staged_str = staged.to_str().ok_or("non-utf8 staging path")?.to_owned();
    tokio::fs::write(&staged, &bytes)
        .await
        .map_err(|e| format!("write staging file: {e}"))?;
    let result = upload_from_path(&staged_str, &mime_type, None).await;
    let _ = tokio::fs::remove_file(&staged).await;
    result
}

pub async fn image_data(blob_id: String) -> Result<Vec<u8>, String> {
    let content_hex = blob_id_sha256_hex(&blob_id)
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
        download_to_path(&blob_id, &path_str, None).await?;
    }
    tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read cached blob: {e}"))
}

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
