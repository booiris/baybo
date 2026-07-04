//! Direct-transport attachments over plain HTTP `/v1/blobs`, authenticated with
//! the stored admin Bearer on the admin listener. The gateway marks these calls
//! as the web identity; device/ios tokens use the relay blob leg instead.

use serde::Deserialize;

use super::INVALID_TOKEN_CODE;

fn admin_context() -> Result<(String, String), String> {
    let creds = super::credentials()?.ok_or("not connected; sign in first")?;
    Ok((creds.base_url, creds.token))
}

/// Upload raw bytes (`POST /v1/blobs`, mime in `content-type`) → content-addressed
/// `blob_id` to reference on the next message.
pub async fn upload_bytes(bytes: Vec<u8>, mime_type: String) -> Result<String, String> {
    let (base, admin_token) = admin_context()?;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/blobs"))
        .bearer_auth(admin_token)
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
pub async fn image_data(blob_id: String) -> Result<Vec<u8>, String> {
    let (base, admin_token) = admin_context()?;
    let mut url = reqwest::Url::parse(&format!("{base}/v1/blobs"))
        .map_err(|e| format!("bad Baybo address: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "bad Baybo address".to_string())?
        .push(&blob_id);
    let resp = reqwest::Client::new()
        .get(url)
        .bearer_auth(admin_token)
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
