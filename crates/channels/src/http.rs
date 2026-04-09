use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use aura_core::{
    AuraError, ChannelType, ContentBlock, IncomingMessage, Message, MessageMetadata,
    OutgoingMessage, User,
};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc, oneshot};
use tower_http::cors::CorsLayer;

use crate::ChannelAdapter;

/// HTTP channel adapter that exposes a REST API via axum.
///
/// - `POST /messages` accepts a JSON request and returns the agent response.
/// - `GET /health` returns 200 OK for liveness checks.
///
/// The adapter bridges HTTP request-response semantics with the push-based
/// `ChannelAdapter` trait by parking each request on a oneshot channel until
/// `send_response` delivers the result.
pub struct HttpChannel {
    bind_addr: String,
    response_timeout: Duration,
    shutdown: Arc<AtomicBool>,
    pending: PendingMap,
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<OutgoingMessage>>>>;

/// JSON body accepted by `POST /messages`.
#[derive(Debug, Deserialize)]
struct MessageRequest {
    session_id: Option<String>,
    user_id: Option<String>,
    content: String,
}

/// JSON body returned by `POST /messages`.
#[derive(Debug, Serialize)]
struct MessageResponse {
    session_id: String,
    content: Vec<ContentBlockJson>,
}

/// Serializable representation of a `ContentBlock`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockJson {
    Text { text: String },
    Image { mime_type: String },
    Audio { mime_type: String },
    File { filename: String, mime_type: String },
}

impl From<&ContentBlock> for ContentBlockJson {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text(text) => ContentBlockJson::Text { text: text.clone() },
            ContentBlock::Image { mime_type, .. } => ContentBlockJson::Image {
                mime_type: mime_type.clone(),
            },
            ContentBlock::Audio { mime_type, .. } => ContentBlockJson::Audio {
                mime_type: mime_type.clone(),
            },
            ContentBlock::File {
                filename,
                mime_type,
                ..
            } => ContentBlockJson::File {
                filename: filename.clone(),
                mime_type: mime_type.clone(),
            },
        }
    }
}

/// Shared state passed into axum handlers.
#[derive(Clone)]
struct AppState {
    sender: mpsc::Sender<IncomingMessage>,
    pending: PendingMap,
    response_timeout: Duration,
}

impl HttpChannel {
    /// Create a new `HttpChannel` that will bind to the given address.
    pub fn new(bind_addr: impl Into<String>) -> Self {
        Self {
            bind_addr: bind_addr.into(),
            response_timeout: Duration::from_secs(30),
            shutdown: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn build_router(&self, sender: mpsc::Sender<IncomingMessage>) -> Router {
        let state = AppState {
            sender,
            pending: Arc::clone(&self.pending),
            response_timeout: self.response_timeout,
        };

        Router::new()
            .route("/messages", routing::post(handle_message))
            .route("/health", routing::get(handle_health))
            .layer(CorsLayer::permissive())
            .with_state(state)
    }
}

impl Default for HttpChannel {
    fn default() -> Self {
        Self::new("0.0.0.0:3000")
    }
}

#[async_trait]
impl ChannelAdapter for HttpChannel {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Http
    }

    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> aura_core::Result<()> {
        let router = self.build_router(sender);
        let shutdown = Arc::clone(&self.shutdown);
        let bind_addr = self.bind_addr.clone();

        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| AuraError::Config(format!("failed to bind to {bind_addr}: {e}")))?;

        tracing::info!("HTTP channel listening on {bind_addr}");

        tokio::spawn(async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    // Poll the shutdown flag; a small tick interval is fine for graceful shutdown.
                    loop {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                })
                .await;

            if let Err(e) = result {
                tracing::error!("HTTP channel server error: {e}");
            }
        });

        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> aura_core::Result<()> {
        let reply_id = response.reply_to.as_deref().ok_or_else(|| {
            AuraError::NotFound("send_response missing reply_to message id".into())
        })?;

        let tx = {
            let mut map = self.pending.lock().await;
            map.remove(reply_id)
        };

        match tx {
            Some(tx) => {
                // The receiver may have timed out already; that is not an error on our side.
                let _ = tx.send(response);
                Ok(())
            }
            None => {
                tracing::warn!("no pending request for message_id={reply_id}");
                Err(AuraError::NotFound(format!(
                    "no pending HTTP request for message_id={reply_id}"
                )))
            }
        }
    }

    async fn stop(&self) -> aura_core::Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        tracing::info!("HTTP channel stopped");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Axum handlers
// ---------------------------------------------------------------------------

async fn handle_health() -> impl IntoResponse {
    StatusCode::OK
}

async fn handle_message(
    State(state): State<AppState>,
    Json(req): Json<MessageRequest>,
) -> impl IntoResponse {
    let message_id = format!("http_{}", uuid::Uuid::new_v4());
    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("http_session_{}", uuid::Uuid::new_v4()));
    let user_id = req.user_id.unwrap_or_else(|| "anonymous".to_owned());

    let incoming = IncomingMessage {
        message: Message {
            id: message_id.clone(),
            session_id: session_id.clone(),
            channel: ChannelType::Http,
            sender: User {
                id: user_id,
                name: None,
                channel: ChannelType::Http,
            },
            content: vec![ContentBlock::Text(req.content)],
            timestamp: chrono::Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        },
    };

    // Register a oneshot so send_response can deliver the reply.
    let (tx, rx) = oneshot::channel();
    {
        let mut map = state.pending.lock().await;
        map.insert(message_id.clone(), tx);
    }

    // Push the incoming message to the agent pipeline.
    if state.sender.send(incoming).await.is_err() {
        let mut map = state.pending.lock().await;
        map.remove(&message_id);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "channel receiver dropped"})),
        );
    }

    // Wait for the agent to call send_response, with a timeout.
    match tokio::time::timeout(state.response_timeout, rx).await {
        Ok(Ok(response)) => {
            let body = MessageResponse {
                session_id: response.session_id,
                content: response
                    .content
                    .iter()
                    .map(ContentBlockJson::from)
                    .collect(),
            };
            (StatusCode::OK, Json(serde_json::json!(body)))
        }
        Ok(Err(_)) => {
            // Sender was dropped without sending a response.
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "response sender dropped"})),
            )
        }
        Err(_) => {
            // Timeout — clean up the pending entry.
            let mut map = state.pending.lock().await;
            map.remove(&message_id);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": "response timed out"})),
            )
        }
    }
}

#[cfg(test)]
impl HttpChannel {
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_channel_default() {
        let channel = HttpChannel::default();
        assert_eq!(channel.bind_addr, "0.0.0.0:3000");
        assert_eq!(channel.response_timeout, Duration::from_secs(30));
    }

    #[test]
    fn channel_type_returns_http() {
        let channel = HttpChannel::default();
        assert_eq!(channel.channel_type(), ChannelType::Http);
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let channel = HttpChannel::default();
        channel.stop().await.expect("first stop should succeed");
        channel.stop().await.expect("second stop should succeed");
        assert!(channel.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn with_timeout_overrides_default() {
        let channel = HttpChannel::new("127.0.0.1:8080").with_timeout(Duration::from_secs(60));
        assert_eq!(channel.response_timeout, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn send_response_without_reply_to_returns_error() {
        let channel = HttpChannel::default();
        let response = OutgoingMessage {
            session_id: "s1".to_owned(),
            channel: ChannelType::Http,
            content: vec![ContentBlock::Text("hi".to_owned())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        };
        let result = channel.send_response(response).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        use tower::ServiceExt;

        let app = HttpChannel::new("0.0.0.0:0").build_router(mpsc::channel(1).0);
        let request = axum::http::Request::builder()
            .uri("/health")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn message_roundtrip() {
        use tower::ServiceExt;

        let (tx, mut rx) = mpsc::channel(16);
        let channel = HttpChannel::new("0.0.0.0:0").with_timeout(Duration::from_secs(2));
        let app = channel.build_router(tx);

        // Spawn a task that receives the incoming message and calls send_response.
        let pending = Arc::clone(&channel.pending);
        tokio::spawn(async move {
            if let Some(incoming) = rx.recv().await {
                let msg_id = incoming.message.id.clone();
                let response = OutgoingMessage {
                    session_id: incoming.message.session_id.clone(),
                    channel: ChannelType::Http,
                    content: vec![ContentBlock::Text("pong".to_owned())],
                    reply_to: Some(msg_id.clone()),
                    metadata: MessageMetadata::default(),
                };
                let tx = {
                    let mut map = pending.lock().await;
                    map.remove(&msg_id)
                };
                if let Some(tx) = tx {
                    let _ = tx.send(response);
                }
            }
        });

        let body = serde_json::json!({"content": "ping"});
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/messages")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "pong");
    }
}
