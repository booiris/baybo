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
        progress: blob_helper::ProgressSink,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + '_ {
        async move { download_blob_with_client(self, blob_id, progress).await }
    }
}

pub(crate) async fn download_blob_bytes(
    sessions: &super::DirectSessions,
    blob_id: String,
    progress: blob_helper::ProgressSink,
) -> Result<Vec<u8>, String> {
    blob_helper::read_or_download_blob_bytes(blob_id, |blob_id, entry| async move {
        let http = sessions.http_client()?;
        download_to_path(&http, &blob_id, &entry, progress).await
    })
    .await
}

async fn download_blob_with_client(
    http: &super::DirectHttp,
    blob_id: String,
    progress: blob_helper::ProgressSink,
) -> Result<Vec<u8>, String> {
    blob_helper::read_or_download_blob_bytes(blob_id, |blob_id, entry| async move {
        download_to_path(http, &blob_id, &entry, progress).await
    })
    .await
}

async fn download_to_path(
    http: &super::DirectHttp,
    blob_id: &str,
    entry: &blob_helper::BlobCacheEntry,
    progress: blob_helper::ProgressSink,
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

    // `content_length` on a 206 counts the REMAINING bytes, so the blob's full
    // length is what a resume already holds plus what this response will carry.
    let total = resp.content_length().map(|len| resume_from + len);
    let mut ticker = blob_helper::ProgressTicker::new(progress, total, resume_from);

    let mut file = blob_helper::open_part_append(entry).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read blob: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write part: {e}"))?;
        hasher.update(&chunk);
        ticker.advance(chunk.len());
    }
    blob_helper::finalize_download(file, entry, hasher).await?;
    ticker.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[derive(Default)]
    struct RecordingProgress {
        ticks: Mutex<Vec<(u64, Option<u64>)>>,
    }

    impl crate::api::BlobProgress for RecordingProgress {
        fn on_progress(&self, downloaded: u64, total: Option<u64>) {
            self.ticks.lock().push((downloaded, total));
        }
    }

    /// Serve `body` once over HTTP/1.1, honouring a `Range: bytes=N-` header
    /// with a 206 so the resume path is exercised for real. Returns the base URL.
    async fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut req = vec![0u8; 2048];
            let n = sock.read(&mut req).await.expect("read request");
            let text = String::from_utf8_lossy(&req[..n]).to_lowercase();
            let from = text
                .split("range: bytes=")
                .nth(1)
                .and_then(|rest| rest.split('-').next())
                .and_then(|n| n.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let slice = &body[from.min(body.len())..];
            let head = if from > 0 {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                    slice.len(),
                    from,
                    body.len() - 1,
                    body.len()
                )
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", slice.len())
            };
            sock.write_all(head.as_bytes()).await.expect("write head");
            sock.write_all(slice).await.expect("write body");
            sock.flush().await.expect("flush");
        });
        format!("http://{addr}")
    }

    fn http_for(base_url: String) -> super::super::DirectHttp {
        // `reqwest::Client::new()` panics without one; the app installs it in
        // `BayboClient::new`, which these unit tests never run.
        let _ = rustls::crypto::ring::default_provider().install_default();
        super::super::DirectHttp {
            base_url,
            device_id: "test-device".into(),
            client: reqwest::Client::new(),
            headers: reqwest::header::HeaderMap::new(),
        }
    }

    /// A blob id whose digest matches `body`, so `finalize_download`'s hash
    /// check passes. The read token is arbitrary — the cache keys on the digest.
    fn blob_id_for(body: &[u8]) -> String {
        format!("sha256:{}.tok", blob_helper::bytes_sha256_hex(body))
    }

    async fn fresh_body(tag: &str) -> Vec<u8> {
        // Unique per run: the on-disk cache is global, and a hit would skip the
        // download this test exists to observe.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        format!("baybo-progress-{tag}-{nonce}")
            .repeat(8)
            .into_bytes()
    }

    #[tokio::test]
    async fn a_real_download_reports_its_first_and_last_byte() {
        let body = fresh_body("plain").await;
        let total = body.len() as u64;
        let http = http_for(serve_once(body.clone()).await);
        let sink = Arc::new(RecordingProgress::default());

        let got = download_blob_with_client(
            &http,
            blob_id_for(&body),
            Some(sink.clone() as Arc<dyn crate::api::BlobProgress>),
        )
        .await
        .expect("download");

        assert_eq!(got, body, "the bytes round-trip");
        let ticks = sink.ticks.lock().clone();
        assert_eq!(
            ticks.first().copied(),
            Some((0, Some(total))),
            "opens at zero with the length from Content-Length; got {ticks:?}"
        );
        assert_eq!(
            ticks.last().copied(),
            Some((total, Some(total))),
            "the final byte always lands; got {ticks:?}"
        );
    }

    /// A resumed download must not rewind the reader's byte counter to zero,
    /// and its `total` must be the whole blob, not the 206's remaining length.
    #[tokio::test]
    async fn a_resumed_download_reports_the_bytes_already_on_disk() {
        let body = fresh_body("resume").await;
        let total = body.len() as u64;
        let resume_from = 40u64;
        let blob_id = blob_id_for(&body);

        // Seed the partial file the way an interrupted download would have.
        let entry = blob_helper::cache_entry(&blob_id).await.expect("entry");
        tokio::fs::write(entry.part_path(), &body[..resume_from as usize])
            .await
            .expect("seed part");

        let http = http_for(serve_once(body.clone()).await);
        let sink = Arc::new(RecordingProgress::default());
        let got = download_blob_with_client(
            &http,
            blob_id,
            Some(sink.clone() as Arc<dyn crate::api::BlobProgress>),
        )
        .await
        .expect("resumed download");

        assert_eq!(got, body);
        let ticks = sink.ticks.lock().clone();
        assert_eq!(
            ticks.first().copied(),
            Some((resume_from, Some(total))),
            "opens at the resume floor, total is the WHOLE blob; got {ticks:?}"
        );
        assert_eq!(ticks.last().copied(), Some((total, Some(total))));
    }
}
