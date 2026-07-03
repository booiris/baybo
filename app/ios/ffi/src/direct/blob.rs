//! Direct-transport attachments over plain HTTP `/v1/blobs`, authenticated with
//! the live session's **channel token** (the admin Bearer is rejected here). The
//! web identity is allowed to upload — device/ios tokens get 403 — which is one
//! reason the direct path registers as a web client.

use serde::Deserialize;

use super::CHANNEL_TOKEN_HEADER;
use super::chat::{DirectSessions, channel_context};

/// Upload raw bytes (`POST /v1/blobs`, mime in `content-type`) → content-addressed
/// `blob_id` to reference on the next message.
pub async fn upload_bytes(
    sessions: &DirectSessions,
    bytes: Vec<u8>,
    mime_type: String,
) -> Result<String, String> {
    let (base, token) = channel_context(sessions).await?;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/blobs"))
        .header(CHANNEL_TOKEN_HEADER, token)
        .header(reqwest::header::CONTENT_TYPE, mime_type)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("upload failed: HTTP {}", resp.status().as_u16()));
    }
    #[derive(Deserialize)]
    struct BlobIdResp {
        blob_id: String,
    }
    let parsed: BlobIdResp = resp
        .json()
        .await
        .map_err(|e| format!("decode blob id: {e}"))?;
    Ok(parsed.blob_id)
}

/// Fetch an attachment (`GET /v1/blobs/{blob_id}`) → raw bytes for the webview to
/// wrap in an object URL. `blob_id` (`sha256:<hex>.<token>`) is pushed as a single
/// path segment so its `:` / `.` / token chars are percent-encoded.
pub async fn image_data(sessions: &DirectSessions, blob_id: String) -> Result<Vec<u8>, String> {
    let (base, token) = channel_context(sessions).await?;
    let mut url = reqwest::Url::parse(&format!("{base}/v1/blobs"))
        .map_err(|e| format!("bad Baybo address: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "bad Baybo address".to_string())?
        .push(&blob_id);
    let resp = reqwest::Client::new()
        .get(url)
        .header(CHANNEL_TOKEN_HEADER, token)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status().as_u16()));
    }
    Ok(resp
        .bytes()
        .await
        .map_err(|e| format!("read blob: {e}"))?
        .to_vec())
}
