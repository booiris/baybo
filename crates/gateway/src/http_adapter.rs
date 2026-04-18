use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::{
    ChannelAdapter, ChannelError, IncomingMessage, NoticeLevel, OutgoingMessage, Result,
};
use aura_model::ChannelType;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast, mpsc};

/// Broadcast capacity per session. Each SSE subscriber keeps a lagging
/// buffer of this many events.
const BROADCAST_CAPACITY: usize = 64;

/// SSE event payload pushed to clients subscribed to a session stream.
///
/// Serde is derived so route handlers can serialise directly to axum SSE
/// events without touching raw JSON strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SseEvent {
    /// Incremental assistant text chunk.
    Delta { text: String },
    /// Final assistant response for the turn (content as rendered text).
    Response { text: String },
    /// Out-of-band notice surfaced by the agent.
    Notice { level: String, text: String },
}

/// HTTP channel adapter.
///
/// Owns a per-session broadcast fan-out. The admin routes register/
/// resubscribe via [`HttpAdapter::subscribe`] when a client opens
/// `GET /v1/sessions/:id/stream`, and submit new user messages through
/// [`HttpAdapter::submit`] from the `POST .../messages` handler.
pub struct HttpAdapter {
    /// Populated on [`start`]; cloned by [`submit`] to push new user
    /// messages into the router.
    incoming_tx: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    /// Per-session broadcast channels. Lazily created on first
    /// [`subscribe`] or first outbound message.
    sessions: RwLock<HashMap<String, broadcast::Sender<SseEvent>>>,
}

impl Default for HttpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpAdapter {
    pub fn new() -> Self {
        Self {
            incoming_tx: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to the SSE stream for a session. Creates the broadcast
    /// channel if this is the first subscriber.
    pub async fn subscribe(&self, session_id: &str) -> broadcast::Receiver<SseEvent> {
        let mut map = self.sessions.write().await;
        match map.get(session_id) {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, rx) = broadcast::channel(BROADCAST_CAPACITY);
                map.insert(session_id.to_owned(), tx);
                rx
            }
        }
    }

    /// Push an inbound user message into the router. Fails if the
    /// adapter has not been started.
    pub async fn submit(&self, msg: IncomingMessage) -> Result<()> {
        let guard = self.incoming_tx.read().await;
        let tx = guard
            .as_ref()
            .ok_or_else(|| ChannelError::Config("adapter not started".into()))?;
        tx.send(msg)
            .await
            .map_err(|e| ChannelError::Config(format!("router intake closed: {e}")))
    }

    async fn broadcast(&self, session_id: &str, event: SseEvent) {
        let map = self.sessions.read().await;
        if let Some(tx) = map.get(session_id) {
            // `send` errors when there are no subscribers — that's not
            // fatal, and the broadcast channel keeps recent events
            // buffered for the next subscriber.
            let _ = tx.send(event);
        }
    }
}

#[async_trait]
impl ChannelAdapter for HttpAdapter {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Http
    }

    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let mut slot = self.incoming_tx.write().await;
        *slot = Some(sender);
        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> Result<()> {
        let text = flatten_content(&response.content);
        self.broadcast(&response.session_id, SseEvent::Response { text })
            .await;
        Ok(())
    }

    async fn send_stream_delta(&self, session_id: &str, delta: &str) -> Result<()> {
        self.broadcast(
            session_id,
            SseEvent::Delta {
                text: delta.to_owned(),
            },
        )
        .await;
        Ok(())
    }

    async fn send_notice(&self, session_id: &str, level: NoticeLevel, text: &str) -> Result<()> {
        let level = match level {
            NoticeLevel::Warn => "warn",
            NoticeLevel::Error => "error",
        };
        self.broadcast(
            session_id,
            SseEvent::Notice {
                level: level.to_owned(),
                text: text.to_owned(),
            },
        )
        .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let mut slot = self.incoming_tx.write().await;
        *slot = None;
        // Dropping broadcast senders signals EOF to all subscribers.
        let mut sessions = self.sessions.write().await;
        sessions.clear();
        Ok(())
    }
}

/// Wrapper for route handlers. The gateway server state holds this.
pub type SharedAdapter = Arc<HttpAdapter>;

fn flatten_content(blocks: &[aura_model::ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let aura_model::ContentBlock::Text(text) = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}
