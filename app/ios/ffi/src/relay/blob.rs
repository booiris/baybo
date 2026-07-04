//! Relay blob transfer over the API tunnel.
//!
//! Each operation uses its own `x-relay-leg-class: blob` data leg, preserving the
//! background bandwidth class and physical isolation from chat. The bytes now
//! ride URL-shaped tunnel messages (`GET/POST /v1/blobs`) instead of the
//! previous bespoke blob protocol.

use device_proto::noise::StaticKeypair;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::pairing::load_paired_record;
use super::tunnel::{
    REQUEST_ID, collect_response_body, dial_tunnel_leg, expect_response_head, next_response,
    send_request,
};
use crate::core::{
    MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest, TunnelResponse, blob_id_sha256_hex,
};

const BLOB_CACHE_SUBDIR: &str = "baybo-blob-cache";
const BLOB_STAGING_SUBDIR: &str = "baybo-blob-staging";
const HEADER_CONTENT_TYPE: &str = "content-type";
const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_CONTENT_SHA256: &str = "x-baybo-content-sha256";
const HEADER_RANGE: &str = "range";

async fn download_to_path(blob_id: &str, dest_path: &str) -> Result<(), String> {
    let expected_hex = blob_id_sha256_hex(blob_id)
        .filter(|hex| is_sha256_hex(hex))
        .ok_or_else(|| "invalid blob id".to_string())?
        .to_owned();
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let (mut ws, mut session) =
        dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Blob).await?;

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
        let _ = ws.close(None).await;
        return Ok(());
    }

    loop {
        match next_response(&mut ws, &mut session).await? {
            TunnelResponse::Body {
                offset, data, last, ..
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
                if last {
                    finalize_download(file, &part_path, dest_path, hasher, &expected_hex).await?;
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

async fn upload_from_path(src_path: &str, mime_type: &str) -> Result<String, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let (sha256_hex, size) = hash_file(src_path).await?;
    let (mut ws, mut session) =
        dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Blob).await?;

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
            if last {
                break;
            }
        }
    }

    let (status, _headers, _body_len) = expect_response_head(&mut ws, &mut session).await?;
    if !(200..300).contains(&status) {
        return Err(format!("upload failed: HTTP {status}"));
    }
    let body = collect_response_body(&mut ws, &mut session, _body_len).await?;
    let _ = ws.close(None).await;
    #[derive(Deserialize)]
    struct BlobIdResp {
        blob_id: String,
    }
    let parsed: BlobIdResp =
        serde_json::from_slice(&body).map_err(|e| format!("decode blob id: {e}"))?;
    Ok(parsed.blob_id)
}

pub(crate) async fn upload_bytes(bytes: Vec<u8>, mime_type: String) -> Result<String, String> {
    let dir = std::env::temp_dir().join(BLOB_STAGING_SUBDIR);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create staging dir: {e}"))?;
    let staged = dir.join(hex::encode(Sha256::digest(&bytes)));
    let staged_str = staged.to_str().ok_or("non-utf8 staging path")?.to_owned();
    tokio::fs::write(&staged, &bytes)
        .await
        .map_err(|e| format!("write staging file: {e}"))?;
    let result = upload_from_path(&staged_str, &mime_type).await;
    let _ = tokio::fs::remove_file(&staged).await;
    result
}

pub(crate) async fn image_data(blob_id: String) -> Result<Vec<u8>, String> {
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
        download_to_path(&blob_id, &path_str).await?;
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
