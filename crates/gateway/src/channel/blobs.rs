//! `POST/GET /v1/blobs/*` — channel-token-authenticated side-channel
//! for non-text media that travels alongside `Frame::Message`.
//!
//! Sidecars `POST /v1/blobs` to upload bytes and receive a content-
//! addressed `blob_id`, then reference that id in
//! [`aura_channels::wire::WireAttachment`]. Outbound, sidecars
//! `GET /v1/blobs/<id>` to pull bytes the agent has produced. Both
//! routes are mounted under the same router as `/channel-ws` so the
//! channel auth middleware ([`crate::auth::channel`]) gates them
//! identically.

use aura_model::ChannelType;
use aura_pairing::CheckOutcome;
use aura_storage::StorageError;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use tokio_util::io::ReaderStream;

use super::state::WsChannelState;
use crate::auth::AuthedClient;

/// Sidecar-supplied originator identity. The sidecar fills these in
/// from the inbound platform event so the gateway can run the same
/// `(channel_type, bot_id, user_id)` pairing check it runs on
/// `Frame::Message` — without these headers a sidecar could persist a
/// 100 MiB blob for an unpaired user before the message-side pairing
/// gate ever runs.
const HEADER_BOT_ID: &str = "x-aura-bot-id";
const HEADER_USER_ID: &str = "x-aura-user-id";

/// Hard cap on a single upload, matched against `DefaultBodyLimit`.
/// Larger blobs would block the WS-paired connection's Tokio worker
/// for the duration of a multi-second I/O — sidecars that need to
/// transfer more than this should chunk on their side.
const MAX_BLOB_BYTES: usize = 100 * 1024 * 1024;

/// JSON returned on a successful upload. Mirrors the shape produced by
/// `BlobStore::put`. Kept tiny by design — sidecars only need the id
/// to embed in `WireAttachment`.
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
    /// Subprocess sidecar; pairing not yet approved. Body contains the
    /// short code the user must run `aura pair approve <code>` for.
    Pending(String),
    /// Hard reject with status + diagnostic body. Used for missing
    /// headers, unbound tokens, pairing errors, etc.
    Reject(StatusCode, &'static str),
}

async fn authorize_upload(
    pairing: &aura_pairing::PairingService,
    authed: &AuthedClient,
    headers: &HeaderMap,
) -> UploadAuth {
    match authed {
        // TUI, web chat, and tool-sidecar uploads are session-scoped
        // (not per-bot/per-user) — pairing doesn't apply.
        AuthedClient::Tui | AuthedClient::Tool { .. } | AuthedClient::Web { .. } => {
            UploadAuth::Bypass
        }
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
                        "missing or empty x-aura-bot-id header",
                    );
                }
            };
            let user_id = match headers.get(HEADER_USER_ID).and_then(|v| v.to_str().ok()) {
                Some(s) if !s.is_empty() => s.to_owned(),
                _ => {
                    return UploadAuth::Reject(
                        StatusCode::BAD_REQUEST,
                        "missing or empty x-aura-user-id header",
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
        .unwrap_or("application/octet-stream")
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

    match state
        .blob_store
        .put_stream(
            stream,
            &mime,
            uploader_identity.as_deref(),
            MAX_BLOB_BYTES as u64,
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
        Err(StorageError::TooLarge { limit, actual }) => {
            tracing::debug!(limit, actual, "blob upload exceeded cap");
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("blob exceeds {limit}-byte cap"),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "blob upload persist failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "blob store failure").into_response()
        }
    }
}

async fn download(State(state): State<WsChannelState>, Path(blob_id): Path<String>) -> Response {
    // `stat` first so missing rows surface as 404 before we hold a
    // file handle; the open path itself also re-verifies, but ordering
    // here lets us send Content-Length up front.
    let meta = match state.blob_store.stat(&blob_id).await {
        Ok(m) => m,
        Err(StorageError::NotFound(_)) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, blob_id, "blob stat failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let reader = match state.blob_store.open(&blob_id).await {
        Ok(r) => r,
        Err(StorageError::NotFound(_)) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, blob_id, "blob open failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let body = Body::from_stream(ReaderStream::new(reader));
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&meta.mime_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&meta.size.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    response
}

#[cfg(test)]
mod tests {
    //! Targeted coverage for `authorize_upload` — the security-critical
    //! step that decides whether the blob bytes are even allowed to
    //! land on disk. Full HTTP round-trip coverage rides with the
    //! channel-WS integration tests; here we just assert the gate
    //! shape without spinning a real server.
    use super::*;
    use aura_pairing::PairingService;
    use aura_storage::ChannelPairingStore;
    use aura_storage::libsql::{LibsqlChannelPairingStore, LibsqlPool};
    use std::sync::Arc;

    async fn fresh_pairing() -> (Arc<dyn ChannelPairingStore>, Arc<PairingService>) {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelPairingStore> = Arc::new(LibsqlChannelPairingStore::new(pool));
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
    async fn subprocess_without_bot_id_is_400() {
        let (_store, svc) = fresh_pairing().await;
        let h = headers_with(&[("x-aura-user-id", "alice")]);
        let out = authorize_upload(&svc, &subprocess("weixin"), &h).await;
        assert!(matches!(
            out,
            UploadAuth::Reject(StatusCode::BAD_REQUEST, _)
        ));
    }

    #[tokio::test]
    async fn subprocess_without_user_id_is_400() {
        let (_store, svc) = fresh_pairing().await;
        let h = headers_with(&[("x-aura-bot-id", "prod-bot")]);
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
        let h = headers_with(&[("x-aura-bot-id", "prod-bot"), ("x-aura-user-id", "alice")]);
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

        let h = headers_with(&[("x-aura-bot-id", "prod-bot"), ("x-aura-user-id", "alice")]);
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
