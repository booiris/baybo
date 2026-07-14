//! `POST/GET /v1/blobs/*` — channel-token-authenticated side-channel
//! for non-text media that travels alongside `Frame::Message`.
//!
//! Sidecars `POST /v1/blobs` to upload bytes and receive a content-
//! addressed `blob_id`, then reference that id in
//! [`baybo_channels::wire::WireAttachment`]. Outbound, sidecars
//! `GET /v1/blobs/<id>` to pull bytes the agent has produced. Both
//! routes are mounted under the same router as `/channel-ws` so the
//! channel auth middleware ([`crate::auth::channel`]) gates them
//! identically.

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use baybo_model::{BlobRef, ChannelType};
use baybo_pairing::CheckOutcome;
use baybo_store::{BlobReader, BlobStore, ByteStream, StorageError, blob::SHA256_PREFIX};
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use tokio_util::io::ReaderStream;

use super::state::WsChannelState;
use crate::auth::AuthedClient;

pub(crate) const MAX_BLOB_BYTES: usize = 100 * 1024 * 1024;
const DEFAULT_BLOB_MIME: &str = "application/octet-stream";
const HEADER_CONTENT_SHA256: &str = "x-baybo-content-sha256";
const DEVICE_UPLOAD_IDENTITY_PREFIX: &str = "device:";

/// Sidecar-supplied originator identity. The sidecar fills these in
/// from the inbound platform event so the gateway can run the same
/// `(channel_type, bot_id, user_id)` pairing check it runs on
/// `Frame::Message` — without these headers a sidecar could persist a
/// 100 MiB blob for an unpaired user before the message-side pairing
/// gate ever runs.
const HEADER_BOT_ID: &str = "x-baybo-bot-id";
const HEADER_USER_ID: &str = "x-baybo-user-id";

/// JSON returned on a successful upload. Kept tiny by design — sidecars only
/// need the id to embed in `WireAttachment`.
#[derive(Debug, Serialize)]
struct UploadResponse {
    blob_id: String,
}

/// JSON returned when the pairing gate refuses an upload. The
/// `code` is the same one [`Frame::Notice`] would carry on the WS
/// path, so the SDK can surface a single user-facing prompt instead
/// of inventing two flavors of "pairing required".
#[derive(Debug, Serialize)]
struct PairingRequiredResponse {
    error: &'static str,
    code: String,
}

/// Outcome of the pre-upload pairing check. Returned by
/// [`authorize_upload`] so the handler stays linear.
#[cfg_attr(test, derive(Debug))]
enum UploadAuth {
    /// TUI / session-scoped client — pairing not applicable.
    Bypass,
    /// Subprocess sidecar passed pairing. Carries the resolved
    /// `(channel_type, bot_id, user_id)` triple — the handler stamps
    /// it onto the blob's `uploader_identity` for diagnostics.
    Approved {
        channel_type: ChannelType,
        bot_id: String,
        user_id: String,
    },
    /// Approved companion device. Device uploads are not per-bot/per-user, but
    /// still carry a durable uploader marker for diagnostics.
    Device { device_id: String },
    /// Subprocess sidecar; pairing not yet approved. Body contains the
    /// short code the user must run `baybo pair approve <code>` for.
    Pending(String),
    /// Hard reject with status + diagnostic body. Used for missing
    /// headers, unbound tokens, pairing errors, etc.
    Reject(StatusCode, &'static str),
}

async fn authorize_upload(
    pairing: &baybo_pairing::PairingService,
    authed: &AuthedClient,
    headers: &HeaderMap,
) -> UploadAuth {
    match authed {
        // TUI, web chat, and tool-sidecar uploads are session-scoped
        // (not per-bot/per-user) — pairing doesn't apply.
        AuthedClient::Tui | AuthedClient::Tool { .. } | AuthedClient::Web => UploadAuth::Bypass,
        AuthedClient::Device { device_id } => UploadAuth::Device {
            device_id: device_id.clone(),
        },
        AuthedClient::Subprocess {
            channel_type: None, ..
        } => UploadAuth::Reject(
            StatusCode::FORBIDDEN,
            "subprocess token has no bound channel_type",
        ),
        AuthedClient::Subprocess {
            channel_type: Some(ct),
            ..
        } => {
            let bot_id = match headers.get(HEADER_BOT_ID).and_then(|v| v.to_str().ok()) {
                Some(s) if !s.is_empty() => s.to_owned(),
                _ => {
                    return UploadAuth::Reject(
                        StatusCode::BAD_REQUEST,
                        "missing or empty x-baybo-bot-id header",
                    );
                }
            };
            let user_id = match headers.get(HEADER_USER_ID).and_then(|v| v.to_str().ok()) {
                Some(s) if !s.is_empty() => s.to_owned(),
                _ => {
                    return UploadAuth::Reject(
                        StatusCode::BAD_REQUEST,
                        "missing or empty x-baybo-user-id header",
                    );
                }
            };
            let channel_type = ChannelType::from(ct.as_str());
            match pairing.check(&channel_type, &bot_id, &user_id).await {
                Ok(CheckOutcome::Approved) => UploadAuth::Approved {
                    channel_type,
                    bot_id,
                    user_id,
                },
                Ok(CheckOutcome::Pending { code }) => UploadAuth::Pending(code),
                Err(e) => {
                    tracing::error!(error = %e, %channel_type, %bot_id, "pairing check failed");
                    UploadAuth::Reject(StatusCode::INTERNAL_SERVER_ERROR, "pairing check failed")
                }
            }
        }
    }
}

pub(crate) fn routes() -> Router<WsChannelState> {
    Router::new()
        .route(
            "/blobs",
            post(upload).layer(DefaultBodyLimit::max(MAX_BLOB_BYTES)),
        )
        .route("/blobs/{blob_id}", get(download))
}

async fn upload(
    State(state): State<WsChannelState>,
    Extension(authed): Extension<AuthedClient>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Pairing first so a flood of unpaired uploads can't cause durable
    // 100 MiB writes — the moment a sidecar holds the bytes, the gate
    // decides whether they're allowed to land at all. TUI traffic is
    // session-scoped and bypasses the gate (same as on the WS side).
    let uploader_identity = match authorize_upload(state.pairing.as_ref(), &authed, &headers).await
    {
        UploadAuth::Bypass => None,
        UploadAuth::Approved {
            channel_type,
            bot_id,
            user_id,
        } => Some(format!("{channel_type}:{bot_id}:{user_id}")),
        UploadAuth::Device { device_id } => {
            Some(format!("{}{}", DEVICE_UPLOAD_IDENTITY_PREFIX, device_id))
        }
        UploadAuth::Pending(code) => {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(PairingRequiredResponse {
                    error: "pairing_required",
                    code,
                }),
            )
                .into_response();
        }
        UploadAuth::Reject(status, msg) => return (status, msg).into_response(),
    };

    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_BLOB_MIME)
        .to_owned();

    // Hand axum's body straight to the blob store as a `Stream<Bytes>`.
    // The store hashes and writes incrementally — peak memory per upload
    // is one chunk (~16 KiB), not the whole 100 MiB body. axum's
    // `DefaultBodyLimit` covers the catastrophic case (a sender that
    // ignores the cap); the trait's own `max_bytes` is the precise
    // arbiter and surfaces TooLarge as 413 here.
    let stream = body
        .into_data_stream()
        .map(|res| match res {
            Ok(chunk) => Ok::<Bytes, std::io::Error>(chunk),
            Err(e) => Err(std::io::Error::other(e)),
        })
        .boxed();

    let claimed_sha256 = match headers
        .get(HEADER_CONTENT_SHA256)
        .and_then(|v| v.to_str().ok())
    {
        Some(value) => match require_sha256_hex(Some(value)) {
            Ok(value) => Some(value),
            Err(err) => return service_error_response(err),
        },
        None => None,
    };

    match put_upload(
        state.blob_store.as_ref(),
        stream,
        &mime,
        uploader_identity.as_deref(),
        claimed_sha256.as_deref(),
    )
    .await
    {
        Ok(blob_ref) => (
            StatusCode::CREATED,
            axum::Json(UploadResponse {
                blob_id: blob_ref.blob_id,
            }),
        )
            .into_response(),
        Err(err) => service_error_response(err),
    }
}

async fn download(
    State(state): State<WsChannelState>,
    Path(blob_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let offset = parse_range_start(
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
    );
    let download = match open_download(state.blob_store.as_ref(), &blob_id, offset).await {
        Ok(download) => download,
        Err(err) => return service_error_response(err),
    };

    let body = Body::from_stream(ReaderStream::new(download.reader));
    let mut response = (download.status, body).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&download.mime_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&download.body_len.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    if let Some(content_range) = download.content_range
        && let Ok(value) = HeaderValue::from_str(&content_range)
    {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
}

struct BlobDownload {
    status: StatusCode,
    mime_type: String,
    body_len: u64,
    content_range: Option<String>,
    reader: BlobReader,
}

#[derive(Debug)]
enum BlobServiceError {
    BadRequest(&'static str),
    NotFound,
    RangeNotSatisfiable(&'static str),
    PayloadTooLarge { limit: u64 },
    ContentHashMismatch,
    StoreFailure,
}

impl BlobServiceError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) | Self::ContentHashMismatch => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::RangeNotSatisfiable(_) => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::StoreFailure => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn client_message(&self) -> String {
        match self {
            Self::BadRequest(reason) | Self::RangeNotSatisfiable(reason) => reason.to_string(),
            Self::NotFound => "blob not found".to_string(),
            Self::PayloadTooLarge { limit } => format!("blob exceeds {limit}-byte cap"),
            Self::ContentHashMismatch => "content hash mismatch".to_string(),
            Self::StoreFailure => "blob store failure".to_string(),
        }
    }
}

fn parse_range_start(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|rest| {
            rest.split_once('-')
                .map_or(rest, |(start, _)| start)
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn require_sha256_hex(value: Option<&str>) -> Result<String, BlobServiceError> {
    match value {
        Some(value) if is_sha256_hex(value) => Ok(value.to_owned()),
        _ => Err(BlobServiceError::BadRequest("missing content hash")),
    }
}

async fn open_download(
    blob_store: &dyn BlobStore,
    blob_id: &str,
    offset: u64,
) -> Result<BlobDownload, BlobServiceError> {
    let meta = match blob_store.stat(blob_id).await {
        Ok(meta) => meta,
        Err(StorageError::NotFound(_)) => return Err(BlobServiceError::NotFound),
        Err(e) => {
            tracing::error!(error = %e, blob_id, "blob stat failed");
            return Err(BlobServiceError::StoreFailure);
        }
    };

    if offset > meta.size {
        return Err(BlobServiceError::RangeNotSatisfiable(
            "range starts past end of blob",
        ));
    }

    let reader = match blob_store.open_at(blob_id, offset).await {
        Ok(reader) => reader,
        Err(StorageError::NotFound(_)) => return Err(BlobServiceError::NotFound),
        Err(e) => {
            tracing::error!(error = %e, blob_id, "blob open failed");
            return Err(BlobServiceError::StoreFailure);
        }
    };

    let body_len = meta.size - offset;
    let status = if offset > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let content_range = (offset > 0).then(|| {
        format!(
            "bytes {offset}-{}/{}",
            meta.size.saturating_sub(1),
            meta.size
        )
    });

    Ok(BlobDownload {
        status,
        mime_type: meta.mime_type,
        body_len,
        content_range,
        reader,
    })
}

async fn put_upload(
    blob_store: &dyn BlobStore,
    stream: ByteStream,
    mime_type: &str,
    uploader_identity: Option<&str>,
    claimed_sha256: Option<&str>,
) -> Result<BlobRef, BlobServiceError> {
    let blob_ref = match blob_store
        .put_stream(stream, mime_type, uploader_identity, MAX_BLOB_BYTES as u64)
        .await
    {
        Ok(blob_ref) => blob_ref,
        Err(StorageError::TooLarge { limit, actual }) => {
            tracing::debug!(limit, actual, "blob upload exceeded cap");
            return Err(BlobServiceError::PayloadTooLarge { limit });
        }
        Err(e) => {
            tracing::error!(error = %e, "blob upload persist failed");
            return Err(BlobServiceError::StoreFailure);
        }
    };

    if let Some(claimed) = claimed_sha256 {
        let got = blob_ref
            .blob_id
            .strip_prefix(SHA256_PREFIX)
            .and_then(|rest| rest.split('.').next())
            .unwrap_or_default();
        if got != claimed {
            if let Err(e) = blob_store.delete(&blob_ref.blob_id).await {
                tracing::warn!(error = %e, "failed to delete hash-mismatched blob");
            }
            return Err(BlobServiceError::ContentHashMismatch);
        }
    }

    Ok(blob_ref)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn service_error_response(err: BlobServiceError) -> Response {
    let status = err.status_code();
    if status == StatusCode::NOT_FOUND {
        return status.into_response();
    }
    (status, err.client_message()).into_response()
}

#[cfg(test)]
mod tests {
    //! Targeted coverage for `authorize_upload` — the security-critical
    //! step that decides whether the blob bytes are even allowed to
    //! land on disk. Full HTTP round-trip coverage rides with the
    //! channel-WS integration tests; here we just assert the gate
    //! shape without spinning a real server.
    use super::*;
    use baybo_pairing::PairingService;
    use baybo_storage::sqlite::{SqliteChannelPairingStore, SqlitePool};
    use baybo_store::ChannelPairingStore;
    use std::sync::Arc;

    async fn fresh_pairing() -> (Arc<dyn ChannelPairingStore>, Arc<PairingService>) {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelPairingStore> = Arc::new(SqliteChannelPairingStore::new(pool));
        let svc = Arc::new(PairingService::new(store.clone()));
        (store, svc)
    }

    fn subprocess(channel_type: &str) -> AuthedClient {
        AuthedClient::Subprocess {
            pid: 1,
            label: format!("sidecar-{channel_type}"),
            channel_type: Some(channel_type.to_string()),
        }
    }

    fn headers_with(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[tokio::test]
    async fn tui_bypasses_pairing() {
        let (_store, svc) = fresh_pairing().await;
        let out = authorize_upload(&svc, &AuthedClient::Tui, &HeaderMap::new()).await;
        assert!(matches!(out, UploadAuth::Bypass));
    }

    /// Tool sidecars (browser MCP server today) are session-scoped
    /// like the TUI, not per-bot/per-user. They must bypass pairing
    /// so screenshot blobs can land before any user has paired.
    #[tokio::test]
    async fn tool_sidecar_bypasses_pairing() {
        let (_store, svc) = fresh_pairing().await;
        let authed = AuthedClient::Tool {
            label: "tool/browser".into(),
        };
        let out = authorize_upload(&svc, &authed, &HeaderMap::new()).await;
        assert!(matches!(out, UploadAuth::Bypass));
    }

    #[tokio::test]
    async fn device_upload_is_allowed_with_device_identity() {
        let (_store, svc) = fresh_pairing().await;
        let out = authorize_upload(
            &svc,
            &AuthedClient::Device {
                device_id: "device-1".into(),
            },
            &HeaderMap::new(),
        )
        .await;
        assert!(matches!(
            out,
            UploadAuth::Device { device_id } if device_id == "device-1"
        ));
    }

    #[tokio::test]
    async fn subprocess_without_bot_id_is_400() {
        let (_store, svc) = fresh_pairing().await;
        let h = headers_with(&[("x-baybo-user-id", "alice")]);
        let out = authorize_upload(&svc, &subprocess("weixin"), &h).await;
        assert!(matches!(
            out,
            UploadAuth::Reject(StatusCode::BAD_REQUEST, _)
        ));
    }

    #[tokio::test]
    async fn subprocess_without_user_id_is_400() {
        let (_store, svc) = fresh_pairing().await;
        let h = headers_with(&[("x-baybo-bot-id", "prod-bot")]);
        let out = authorize_upload(&svc, &subprocess("weixin"), &h).await;
        assert!(matches!(
            out,
            UploadAuth::Reject(StatusCode::BAD_REQUEST, _)
        ));
    }

    #[tokio::test]
    async fn unbound_subprocess_token_is_403() {
        let (_store, svc) = fresh_pairing().await;
        let authed = AuthedClient::Subprocess {
            pid: 1,
            label: "unbound".into(),
            channel_type: None,
        };
        let out = authorize_upload(&svc, &authed, &HeaderMap::new()).await;
        assert!(matches!(out, UploadAuth::Reject(StatusCode::FORBIDDEN, _)));
    }

    #[tokio::test]
    async fn unpaired_triple_pending_with_code() {
        // Fresh pairing store: an unknown triple causes
        // `PairingService::check` to mint a code and return Pending —
        // the upload must refuse so no bytes hit disk.
        let (_store, svc) = fresh_pairing().await;
        let h = headers_with(&[("x-baybo-bot-id", "prod-bot"), ("x-baybo-user-id", "alice")]);
        match authorize_upload(&svc, &subprocess("weixin"), &h).await {
            UploadAuth::Pending(code) => assert!(!code.is_empty(), "minted code"),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn approved_triple_passes_with_resolved_identity() {
        let (store, svc) = fresh_pairing().await;
        let now = chrono::Utc::now().timestamp();
        // Pre-approve so check() returns Approved on the first call.
        let row = store
            .upsert_pending(
                &ChannelType::from("weixin"),
                "prod-bot",
                "alice",
                "TESTCD",
                now,
                now + 600,
            )
            .await
            .unwrap();
        store.approve_by_code(&row.code, now).await.unwrap();

        let h = headers_with(&[("x-baybo-bot-id", "prod-bot"), ("x-baybo-user-id", "alice")]);
        match authorize_upload(&svc, &subprocess("weixin"), &h).await {
            UploadAuth::Approved {
                bot_id, user_id, ..
            } => {
                assert_eq!(bot_id, "prod-bot");
                assert_eq!(user_id, "alice");
            }
            other => panic!("expected Approved, got {other:?}"),
        }
    }
}
