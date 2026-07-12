//! Relay blob transfer over the API tunnel.
//!
//! Each operation uses its own `x-relay-leg-class: blob` data leg, preserving the
//! background bandwidth class and physical isolation from chat. The bytes now
//! ride URL-shaped tunnel messages (`GET/POST /v1/blobs`) instead of the
//! previous bespoke blob protocol.

use device_proto::noise::StaticKeypair;
use serde::Deserialize;
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use super::pairing::load_paired_record;
use super::tunnel::{BLOB_REQUEST_ID, body_frames, dial_tunnel_leg};
use crate::blob_helper;
use crate::core::{TunnelHeader, TunnelRequest, TunnelResponse};
use crate::gateway_api::{GatewayBlobClient, PATH_BLOBS};

const HEADER_CONTENT_TYPE: &str = "content-type";
const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_CONTENT_SHA256: &str = "x-baybo-content-sha256";
const HEADER_RANGE: &str = "range";

async fn download_to_path(
    blob_id: &str,
    entry: &blob_helper::BlobCacheEntry,
    progress: blob_helper::ProgressSink,
) -> Result<(), String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let mut leg =
        dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Blob).await?;

    let (mut hasher, resume_from) = blob_helper::hash_existing_part(entry).await?;
    let mut headers = Vec::new();
    if resume_from > 0 {
        headers.push(TunnelHeader::new(
            HEADER_RANGE,
            format!("bytes={resume_from}-"),
        ));
    }
    leg.send(&TunnelRequest::Head {
        request_id: BLOB_REQUEST_ID,
        method: "GET".into(),
        path: format!("{PATH_BLOBS}/{blob_id}"),
        headers,
        body_len: None,
    })
    .await?;

    let head = leg.expect_response_head(BLOB_REQUEST_ID).await?;
    if !(head.status == 200 || head.status == 206) {
        return Err(format!("download failed: HTTP {}", head.status));
    }
    if resume_from > 0 && head.status != 206 {
        return Err("download resume was not accepted".into());
    }

    // A 206's `body_len` counts the REMAINING bytes, so the blob's full length
    // is what a resume already holds plus what this response will carry.
    let total = head.body_len.map(|len| resume_from + len);
    let mut ticker = blob_helper::ProgressTicker::new(progress, total, resume_from);

    let mut file = blob_helper::open_part_append(entry).await?;

    let mut expected_body_offset = resume_from;
    if head.body_len == Some(0) {
        blob_helper::finalize_download(file, entry, hasher).await?;
        ticker.finish();
        leg.close().await;
        return Ok(());
    }

    // Streamed to disk rather than collected: a blob runs to 100 MiB, so this is
    // the one tunnel response we cannot buffer whole.
    loop {
        match leg.next_response().await? {
            TunnelResponse::Body {
                request_id,
                offset,
                data,
                last,
            } => {
                if request_id != BLOB_REQUEST_ID {
                    drop(file);
                    blob_helper::remove_part(entry).await;
                    return Err(format!("blob body for request {request_id}"));
                }
                if offset != expected_body_offset {
                    drop(file);
                    blob_helper::remove_part(entry).await;
                    return Err(format!(
                        "body offset {offset} != expected {expected_body_offset}",
                    ));
                }
                file.write_all(&data)
                    .await
                    .map_err(|e| format!("write part: {e}"))?;
                hasher.update(&data);
                expected_body_offset += data.len() as u64;
                ticker.advance(data.len());
                if last {
                    blob_helper::finalize_download(file, entry, hasher).await?;
                    ticker.finish();
                    leg.close().await;
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

async fn upload_bytes(bytes: Vec<u8>, mime_type: String) -> Result<String, String> {
    let expected_hex = blob_helper::bytes_sha256_hex(&bytes);
    let blob_id = upload_bytes_over_tunnel(&bytes, &mime_type, &expected_hex).await?;
    blob_helper::cache_uploaded_bytes_best_effort(&expected_hex, &bytes).await;
    Ok(blob_id)
}

async fn upload_bytes_over_tunnel(
    bytes: &[u8],
    mime_type: &str,
    expected_hex: &str,
) -> Result<String, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let size = bytes.len() as u64;
    let mut leg =
        dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Blob).await?;

    leg.send(&TunnelRequest::Head {
        request_id: BLOB_REQUEST_ID,
        method: "POST".into(),
        path: PATH_BLOBS.into(),
        headers: vec![
            TunnelHeader::new(HEADER_CONTENT_TYPE, mime_type),
            TunnelHeader::new(HEADER_CONTENT_LENGTH, size.to_string()),
            TunnelHeader::new(HEADER_CONTENT_SHA256, expected_hex),
        ],
        body_len: Some(size),
    })
    .await?;

    for frame in body_frames(BLOB_REQUEST_ID, bytes) {
        leg.send(&frame).await?;
    }

    let head = leg.expect_response_head(BLOB_REQUEST_ID).await?;
    let body = leg
        .collect_response_body(BLOB_REQUEST_ID, head.body_len)
        .await?;
    leg.close().await;
    if !(200..300).contains(&head.status) {
        return Err(format!("upload failed: HTTP {}", head.status));
    }
    #[derive(Deserialize)]
    struct BlobIdResp {
        blob_id: String,
    }
    let parsed: BlobIdResp =
        serde_json::from_slice(&body).map_err(|e| format!("decode blob id: {e}"))?;
    blob_helper::ensure_blob_id_matches(expected_hex, &parsed.blob_id)?;
    Ok(parsed.blob_id)
}

async fn download_blob_bytes(
    blob_id: String,
    progress: blob_helper::ProgressSink,
) -> Result<Vec<u8>, String> {
    blob_helper::read_or_download_blob_bytes(blob_id, |blob_id, entry| async move {
        download_to_path(&blob_id, &entry, progress).await
    })
    .await
}

#[allow(clippy::manual_async_fn)]
impl GatewayBlobClient for super::GatewayApi {
    fn upload_blob(
        &self,
        bytes: Vec<u8>,
        mime_type: String,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send + '_ {
        async move { upload_bytes(bytes, mime_type).await }
    }

    fn download_blob(
        &self,
        blob_id: String,
        progress: blob_helper::ProgressSink,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + '_ {
        async move { download_blob_bytes(blob_id, progress).await }
    }
}
