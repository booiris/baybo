use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use aura_channels::{
    AgentOutput, ApprovalEvent, ChannelAdapter, ChannelError, IncomingMessage, NoticeLevel, Result,
    SseEvent,
};
use aura_model::ChannelType;
use aura_tools::{ApprovalDecision, ApprovalGate, ApprovalQueue, ChannelApprovalGate};
use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc};

/// How long the approval gate waits for a REST client to `POST
/// /v1/approvals/:id` before failing closed. Matches the TUI's 5-minute
/// timeout; long enough that a human has time to read + react, short
/// enough that forgotten prompts don't pin tool executors forever.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Broadcast capacity for the gateway-wide approval event stream.
/// Separate from per-session SSE because approvals are keyed by
/// `ChannelType::Http`, not by session; every HTTP subscriber sees the
/// same approval lifecycle.
const APPROVAL_STREAM_CAPACITY: usize = 32;

/// Broadcast capacity per session. Each SSE subscriber keeps a lagging
/// buffer of this many events.
const BROADCAST_CAPACITY: usize = 64;

/// HTTP channel adapter.
///
/// Owns a per-session broadcast fan-out. The admin routes register/
/// resubscribe via [`HttpAdapter::subscribe`] when a client opens
/// `GET /v1/sessions/:id/stream`, and submit new user messages through
/// [`HttpAdapter::submit`] from the `POST .../messages` handler.
///
/// Also owns the channel-wide approval queue + broadcast stream used
/// by `/v1/approvals*` — approvals key on `ChannelType::Http`, not on
/// a specific session, so they live here rather than in `sessions`.
pub struct HttpAdapter {
    /// Populated on [`start`]; cloned by [`submit`] to push new user
    /// messages into the router. `OnceLock` models the real lifecycle —
    /// set exactly once at `start()` — without paying for a `RwLock`.
    /// `stop()` can no longer clear this slot; closing the mpsc sender
    /// happens when the adapter itself is dropped, which matches how
    /// the registry handles shutdown today.
    incoming_tx: OnceLock<mpsc::Sender<IncomingMessage>>,
    /// Per-session broadcast channels. Lazily created on first
    /// [`subscribe`] or first outbound message.
    sessions: DashMap<String, broadcast::Sender<SseEvent>>,
    /// Pending tool-approval queue. Fed by [`approval_gate`]'s
    /// [`ChannelApprovalGate`]; drained by the `/v1/approvals` REST
    /// handlers.
    approval_queue: ApprovalQueue,
    /// Gateway-wide broadcast of [`ApprovalEvent`]s. Every SSE
    /// subscriber on `/v1/approvals/stream` sees the same events — the
    /// TUI is typically the only one, but other admin UIs may attach.
    approval_stream: broadcast::Sender<ApprovalEvent>,
}

impl Default for HttpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpAdapter {
    pub fn new() -> Self {
        let (approval_stream, _rx) = broadcast::channel(APPROVAL_STREAM_CAPACITY);
        Self {
            incoming_tx: OnceLock::new(),
            sessions: DashMap::new(),
            approval_queue: ApprovalQueue::new(),
            approval_stream,
        }
    }

    /// Subscribe to the SSE stream for a session. Creates the broadcast
    /// channel if this is the first subscriber.
    pub async fn subscribe(&self, session_id: &str) -> broadcast::Receiver<SseEvent> {
        use dashmap::mapref::entry::Entry;
        match self.sessions.entry(session_id.to_owned()) {
            Entry::Occupied(e) => e.get().subscribe(),
            Entry::Vacant(e) => {
                let (tx, rx) = broadcast::channel(BROADCAST_CAPACITY);
                e.insert(tx);
                rx
            }
        }
    }

    /// Subscribe to the gateway-wide approval event stream.
    pub fn subscribe_approvals(&self) -> broadcast::Receiver<ApprovalEvent> {
        self.approval_stream.subscribe()
    }

    /// Access the shared approval queue so REST handlers can list and
    /// resolve entries.
    pub fn approval_queue(&self) -> &ApprovalQueue {
        &self.approval_queue
    }

    /// Publish a resolution event to the approval stream. Used by the
    /// `POST /v1/approvals/:id` handler after it calls
    /// `ApprovalQueue::resolve_by_call_id` so all connected subscribers
    /// (e.g. a second TUI) drop the entry from their UI.
    pub fn publish_approval_resolved(&self, call_id: String, decision: ApprovalDecision) {
        let _ = self
            .approval_stream
            .send(ApprovalEvent::Resolved { call_id, decision });
    }

    /// Push an inbound user message into the router. Fails if the
    /// adapter has not been started.
    pub async fn submit(&self, msg: IncomingMessage) -> Result<()> {
        let tx = self
            .incoming_tx
            .get()
            .ok_or_else(|| ChannelError::Config("adapter not started".into()))?;
        tx.send(msg)
            .await
            .map_err(|e| ChannelError::Config(format!("router intake closed: {e}")))
    }

    fn broadcast(&self, session_id: &str, event: SseEvent) {
        if let Some(entry) = self.sessions.get(session_id) {
            // `send` errors when there are no subscribers — that's not
            // fatal, and the broadcast channel keeps recent events
            // buffered for the next subscriber.
            let _ = entry.send(event);
        }
    }
}

#[async_trait]
impl ChannelAdapter for HttpAdapter {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Http
    }

    fn approval_gate(&self) -> Option<Arc<dyn ApprovalGate>> {
        let queue = self.approval_queue.clone();
        let stream = self.approval_stream.clone();
        let gate = ChannelApprovalGate::new(
            queue.clone(),
            Arc::new(move || {
                // The waker fires sync-ly from inside
                // `ChannelApprovalGate::request`, immediately after the
                // entry is pushed. Snapshot the tail to catch the push
                // that just happened and broadcast it — listeners use
                // this as a hint to redraw, and do a fresh
                // `GET /v1/approvals` to reconcile state. If two
                // approvals push concurrently the stream may emit them
                // out of order, but the REST list remains authoritative.
                let last = queue.list().into_iter().next_back();
                if let Some(req) = last {
                    let _ = stream.send(ApprovalEvent::Added {
                        call_id: req.call_id,
                        session_id: req.session_id,
                        tool: req.tool,
                        accesses: req.accesses,
                        params_preview: req.params_preview,
                    });
                }
            }),
            APPROVAL_TIMEOUT,
        );
        Some(Arc::new(gate))
    }

    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        self.incoming_tx
            .set(sender)
            .map_err(|_| ChannelError::Config("adapter already started".into()))
    }

    async fn send(&self, output: AgentOutput) -> Result<()> {
        match output {
            AgentOutput::Delta {
                session_id, text, ..
            } => {
                self.broadcast(&session_id, SseEvent::Delta { text });
            }
            AgentOutput::Message(response) => {
                let text = flatten_content(&response.content);
                self.broadcast(&response.session_id, SseEvent::Response { text });
            }
            AgentOutput::Notice {
                session_id,
                level,
                text,
                ..
            } => {
                let level = match level {
                    NoticeLevel::Warn => "warn",
                    NoticeLevel::Error => "error",
                };
                self.broadcast(
                    &session_id,
                    SseEvent::Notice {
                        level: level.to_owned(),
                        text,
                    },
                );
            }
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Dropping broadcast senders signals EOF to all subscribers.
        // `incoming_tx` stays set — see the field doc for why.
        self.sessions.clear();
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
