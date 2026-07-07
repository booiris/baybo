//! Direct blob transfer over the authenticated REST client.

use futures_util::StreamExt;
use serde::Deserialize;
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use crate::blob_helper;
use crate::gateway_api::{GatewayBlobClient, PATH_BLOBS};

use super::INVALID_TOKEN_CODE;

#[allow(clippy::manual_async_fn)]
impl GatewayBlobClient for super::DirectHttp {
    fn upload_blob(
        &self,
        bytes: Vec<u8>,
        mime_type: String,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send + '_ {
        async move {
            let body = bytes::Bytes::from(bytes);
            let expected_hex = blob_helper::bytes_sha256_hex(&body);
            let resp = self
                .client()
                .post(self.url(PATH_BLOBS))
                .header(reqwest::header::CONTENT_TYPE, mime_type)
                .body(body.clone())
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
            blob_helper::ensure_blob_id_matches(&expected_hex, &parsed.blob_id)?;
            blob_helper::cache_uploaded_bytes_best_effort(&expected_hex, &body).await;
            Ok(parsed.blob_id)
        }
    }

    fn download_blob(
        &self,
        blob_id: String,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + '_ {
        async move { download_blob_with_client(self, blob_id).await }
    }
}

pub(crate) async fn download_blob_bytes(
    sessions: &super::DirectSessions,
    blob_id: String,
) -> Result<Vec<u8>, String> {
    blob_helper::read_or_download_blob_bytes(blob_id, |blob_id, entry| async move {
        let http = sessions.http_client()?;
        download_to_path(&http, &blob_id, &entry).await
    })
    .await
}

async fn download_blob_with_client(
    http: &super::DirectHttp,
    blob_id: String,
) -> Result<Vec<u8>, String> {
    blob_helper::read_or_download_blob_bytes(blob_id, |blob_id, entry| async move {
        download_to_path(http, &blob_id, &entry).await
    })
    .await
}

async fn download_to_path(
    http: &super::DirectHttp,
    blob_id: &str,
    entry: &blob_helper::BlobCacheEntry,
) -> Result<(), String> {
    let mut url = reqwest::Url::parse(&http.url(PATH_BLOBS))
        .map_err(|e| format!("bad Baybo address: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "bad Baybo address".to_string())?
        .push(blob_id);
    let (mut hasher, resume_from) = blob_helper::hash_existing_part(entry).await?;
    let mut req = http.client().get(url);
    if resume_from > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !(resp.status() == reqwest::StatusCode::OK
        || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT)
    {
        return Err(format!("download failed: HTTP {}", resp.status().as_u16()));
    }
    if resume_from > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err("download resume was not accepted".into());
    }

    let mut file = blob_helper::open_part_append(entry).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read blob: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write part: {e}"))?;
        hasher.update(&chunk);
    }
    blob_helper::finalize_download(file, entry, hasher).await
}
