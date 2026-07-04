//! Direct-transport attachments over plain HTTP `/v1/blobs`, authenticated with
//! the stored gateway Bearer plus device id header on the admin listener. The
//! gateway marks these calls as the direct device identity.

use serde::Deserialize;

use super::INVALID_TOKEN_CODE;

/// Upload raw bytes (`POST /v1/blobs`, mime in `content-type`) → content-addressed
/// `blob_id` to reference on the next message.
pub async fn upload_bytes(
    sessions: &super::DirectSessions,
    bytes: Vec<u8>,
    mime_type: String,
) -> Result<String, String> {
    let http = sessions.http_client()?;
    let resp = http
        .client()
        .post(http.url("/v1/blobs"))
        .header(reqwest::header::CONTENT_TYPE, mime_type)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
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
pub async fn image_data(
    sessions: &super::DirectSessions,
    blob_id: String,
) -> Result<Vec<u8>, String> {
    let http = sessions.http_client()?;
    let mut url = reqwest::Url::parse(&http.url("/v1/blobs"))
        .map_err(|e| format!("bad Baybo address: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "bad Baybo address".to_string())?
        .push(&blob_id);
    let resp = http
        .client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status().as_u16()));
    }
    Ok(resp
        .bytes()
        .await
        .map_err(|e| format!("read blob: {e}"))?
        .to_vec())
}
