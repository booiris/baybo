//! Direct blob transfer over the authenticated REST client.

use std::path::Path;

use futures_util::StreamExt;
use serde::Deserialize;
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use crate::blob_helper;
use crate::gateway_api::{GatewayBlobClient, PATH_BLOBS};

use super::INVALID_TOKEN_CODE;

#[derive(Deserialize)]
struct BlobIdResp {
    blob_id: String,
}

#[allow(clippy::manual_async_fn)]
impl GatewayBlobClient for super::DirectHttp {
    fn upload_blob(
        &self,
        bytes: Vec<u8>,
        mime_type: String,
        deck_card: Option<String>,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send + '_ {
        async move {
            let body = bytes::Bytes::from(bytes);
            let expected_hex = blob_helper::bytes_sha256_hex(&body);
            let mut req = self
                .client()
                .post(self.url(PATH_BLOBS))
                .header(reqwest::header::CONTENT_TYPE, mime_type)
                .body(body.clone());
            if let Some(card_id) = &deck_card {
                req = req.header(crate::gateway_api::HEADER_DECK_CARD, card_id);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| format!("could not reach Baybo: {e}"))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(INVALID_TOKEN_CODE.into());
            }
            if !resp.status().is_success() {
                return Err(format!("upload failed: HTTP {}", resp.status().as_u16()));
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

    fn upload_blob_file(
        &self,
        path: String,
        mime_type: String,
        deck_card: Option<String>,
        progress: blob_helper::ProgressSink,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send + '_ {
        async move { upload_file_with_client(self, path, mime_type, deck_card, progress).await }
    }

    fn download_blob(
        &self,
        blob_id: String,
        progress: blob_helper::ProgressSink,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + '_ {
        async move { download_blob_with_client(self, blob_id, progress).await }
    }
}

async fn upload_file_with_client(
    http: &super::DirectHttp,
    path: String,
    mime_type: String,
    deck_card: Option<String>,
    progress: blob_helper::ProgressSink,
) -> Result<String, String> {
    let source = Path::new(&path);
    let digest = blob_helper::hash_upload_file(source).await?;
    let ticker = blob_helper::UploadTicker::new(progress, digest.len);
    let tee = blob_helper::UploadCacheTee::open(&digest.hex).await;
    let reader = blob_helper::UploadReader::open(
        source,
        digest.len,
        blob_helper::FILE_READ_CHUNK_BYTES,
        ticker.clone(),
        tee.clone(),
    )
    .await?;
    let body = reqwest::Body::wrap_stream(futures_util::stream::try_unfold(
        reader,
        |mut reader| async move {
            match reader.next_chunk().await? {
                Some(chunk) => Ok::<_, String>(Some((bytes::Bytes::from(chunk), reader))),
                None => Ok(None),
            }
        },
    ));
    // A wrapped stream declares no length, and reqwest would fall back to
    // chunked transfer-encoding — which the gateway's body limit reads as an
    // undeclared size.
    let mut req = http
        .client()
        .post(http.url(PATH_BLOBS))
        .header(reqwest::header::CONTENT_TYPE, mime_type)
        .header(reqwest::header::CONTENT_LENGTH, digest.len)
        .body(body);
    if let Some(card_id) = &deck_card {
        req = req.header(crate::gateway_api::HEADER_DECK_CARD, card_id);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("upload failed: HTTP {}", resp.status().as_u16()));
    }
    ticker.finish();
    let parsed: BlobIdResp = resp
        .json()
        .await
        .map_err(|e| format!("decode blob id: {e}"))?;
    blob_helper::ensure_blob_id_matches(&digest.hex, &parsed.blob_id)?;
    tee.publish().await;
    Ok(parsed.blob_id)
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

    /// What one upload request actually put on the wire.
    struct RecordedUpload {
        content_length: Option<usize>,
        content_type: Option<String>,
        transfer_encoding: Option<String>,
        deck_card: Option<String>,
        body: Vec<u8>,
    }

    /// Accept ONE `POST /v1/blobs`, answer with `blob_id`, and hand back the
    /// request it saw.
    async fn serve_upload_once(
        blob_id: String,
    ) -> (String, tokio::sync::oneshot::Receiver<RecordedUpload>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut raw = Vec::new();
            let mut buf = vec![0u8; 16 * 1024];
            let head_end = loop {
                let n = sock.read(&mut buf).await.expect("read request");
                assert!(n > 0, "client closed before the request head");
                raw.extend_from_slice(&buf[..n]);
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&raw[..head_end]).to_lowercase();
            let header = |name: &str| {
                let prefix = format!("{name}: ");
                head.lines()
                    .find_map(|line| line.strip_prefix(prefix.as_str()))
                    .map(|value| value.trim().to_string())
            };
            let content_length = header("content-length").and_then(|v| v.parse::<usize>().ok());
            let mut body = raw[head_end..].to_vec();
            while content_length.is_some_and(|len| body.len() < len) {
                let n = sock.read(&mut buf).await.expect("read body");
                assert!(n > 0, "client closed mid-body");
                body.extend_from_slice(&buf[..n]);
            }
            let payload = format!(r#"{{"blob_id":"{blob_id}"}}"#);
            sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    payload.len()
                )
                .as_bytes(),
            )
            .await
            .expect("write head");
            sock.write_all(payload.as_bytes())
                .await
                .expect("write body");
            sock.flush().await.expect("flush");
            let _ = tx.send(RecordedUpload {
                content_length,
                content_type: header("content-type"),
                transfer_encoding: header("transfer-encoding"),
                deck_card: header(crate::gateway_api::HEADER_DECK_CARD),
                body,
            });
        });
        (format!("http://{addr}"), rx)
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

    /// A picked file on disk, several read chunks long, that no other test can
    /// collide with.
    async fn scratch_upload(tag: &str) -> (std::path::PathBuf, Vec<u8>) {
        let bytes = fresh_body(tag).await.repeat(600);
        assert!(bytes.len() > blob_helper::FILE_READ_CHUNK_BYTES * 2);
        let path =
            std::env::temp_dir().join(format!("baybo-direct-upload-{tag}-{}", std::process::id()));
        tokio::fs::write(&path, &bytes).await.expect("seed file");
        (path, bytes)
    }

    /// The streaming leg's contract in one pass: an explicit `Content-Length`
    /// (never chunked, which the gateway's body limit reads as an undeclared
    /// size), the bytes verbatim across several chunks, ticks that open and
    /// close on the real byte count, and — the part that decides whether the
    /// message renders or shows a download arrow — the source file landed in
    /// the blob cache under its digest.
    #[tokio::test]
    async fn a_streamed_upload_declares_its_length_and_caches_what_it_sent() {
        let (path, bytes) = scratch_upload("streamed").await;
        let total = bytes.len() as u64;
        let blob_id = blob_id_for(&bytes);
        let (base_url, recorded) = serve_upload_once(blob_id.clone()).await;
        let sink = Arc::new(RecordingProgress::default());

        let got = upload_file_with_client(
            &http_for(base_url),
            path.to_string_lossy().into_owned(),
            "application/pdf".into(),
            None,
            Some(sink.clone() as Arc<dyn crate::api::BlobProgress>),
        )
        .await
        .expect("upload");

        assert_eq!(got, blob_id);
        let seen = recorded.await.expect("recorded");
        assert_eq!(seen.content_length, Some(bytes.len()));
        assert_eq!(
            seen.transfer_encoding, None,
            "a declared length, not chunked"
        );
        assert_eq!(seen.content_type.as_deref(), Some("application/pdf"));
        assert_eq!(seen.deck_card, None, "a chat upload stamps no card");
        assert_eq!(seen.body, bytes);

        let ticks = sink.ticks.lock().clone();
        assert_eq!(ticks.first().copied(), Some((0, Some(total))));
        assert_eq!(ticks.last().copied(), Some((total, Some(total))));
        assert!(
            blob_helper::is_cached(&blob_id).await,
            "the file just sent must read as ready"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// A deck picker's upload carries the card stamp, so card purge reclaims it.
    #[tokio::test]
    async fn a_streamed_deck_upload_stamps_its_card() {
        let (path, bytes) = scratch_upload("deck").await;
        let (base_url, recorded) = serve_upload_once(blob_id_for(&bytes)).await;

        upload_file_with_client(
            &http_for(base_url),
            path.to_string_lossy().into_owned(),
            "image/png".into(),
            Some("card-1".into()),
            None,
        )
        .await
        .expect("upload");

        assert_eq!(
            recorded.await.expect("recorded").deck_card.as_deref(),
            Some("card-1")
        );
        let _ = tokio::fs::remove_file(&path).await;
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
