//! Per-WebSocket gateway state.
//!
//! After the Channel / Connection / Subscription refactor each WS
//! upgrade produces one [`WsConnection`] that:
//!
//! * Looks up the per-type [`Channel`] in the workspace
//!   [`ChannelRegistry`] (the registry is populated at gateway boot
//!   from `ChannelsConfig`; lazy install runs here as a fallback for
//!   test fixtures that skipped the boot path).
//! * Builds a [`Connection`] backed by per-WS outbound mpscs and
//!   attaches it to the channel.
//! * Spawns two tasks owned by this connection: a translator
//!   ([`SessionEvent`] → [`Frame`]) and an outbound pump
//!   ([`Frame`] → WebSocket bytes).
//!
//! All wire-format knowledge stays here; `aura-channels` never sees
//! `Frame`.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::wire::{
    self, AttachmentKind, Frame, Message as WireMessage, TaskView, WireAttachment,
};
use aura_channels::{
    AgentEvent, AgentOutput, Channel, ChannelError, ChannelRegistry, Connection, ConnectionId,
    ConnectionSink, MessageRole, NoticeLevel, SendOutcome, SessionEvent, ToolStatus, TurnStatus,
};
use aura_model::{ChannelType, ContentBlock};
use aura_store::BlobStore;
use aura_tools::ApprovalDecision;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket};
use futures::SinkExt;
use futures::stream::SplitSink;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub(crate) type WsSink = SplitSink<WebSocket, AxumWsMessage>;

/// Outbound mpsc buffer. Large enough to absorb a short burst of
/// deltas without back-pressuring the agent loop; small enough that a
/// dead WS sink drops frames rather than piling up unbounded.
const OUTBOUND_BUFFER: usize = 64;

/// Per-WebSocket connection state. One per accepted `/v1/channel-ws`
/// client. Holds the channel-side handle, the outbound frame sender
/// for control pushes, and the pump task handle.
pub(crate) struct Sidecar {
    pub channel: Arc<Channel>,
    pub connection: Arc<Connection>,
    frame_tx: mpsc::Sender<Frame>,
    pump: JoinHandle<()>,
}

/// Resolve the channel for `channel_type` from the registry, falling
/// back to a lazy install for out-of-tree sidecar channels that the
/// boot-time installer didn't know about (custom platforms declared
/// via `aura.json`).
///
/// Split out from [`Sidecar::build`] so the route handler can run
/// this *before* committing the sink to the build path. On `Err`,
/// the route handler still owns the sink and can write a
/// `Frame::RegisterAck { ok: false, reason }` to surface the failure
/// to the peer — a silent close (which is what `build` did on the
/// `?` early-return) leaves the client without a recoverable signal.
pub(crate) fn resolve_or_install_channel(
    registry: &Arc<ChannelRegistry>,
    channel_type: &ChannelType,
) -> Result<Arc<Channel>, ChannelError> {
    if let Some(ch) = registry.get(channel_type) {
        return Ok(ch);
    }
    super::boot::install_channel(registry, channel_type.clone())?;
    registry.get(channel_type).ok_or_else(|| {
        ChannelError::Config(format!("channel '{channel_type}' missing after install"))
    })
}

impl Sidecar {
    /// Build the per-WS state from an already-resolved channel handle.
    /// Infallible — all the failure modes the previous `build`
    /// surfaced lived in the channel resolve, which now runs in
    /// [`resolve_or_install_channel`] so the route can ack failures
    /// on the wire instead of dropping the socket silently.
    pub(crate) fn build(
        channel_type: ChannelType,
        channel: Arc<Channel>,
        sink: WsSink,
        blob_store: Arc<dyn BlobStore>,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(OUTBOUND_BUFFER);
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(OUTBOUND_BUFFER);

        // Translator: SessionEvent → Frame. Exits when every clone of
        // `event_tx` drops (channel detach + sidecar drop).
        let translator_tx = frame_tx.clone();
        let translator_blobs = Arc::clone(&blob_store);
        tokio::spawn(translator_loop(
            event_rx,
            translator_tx,
            channel_type,
            translator_blobs,
        ));

        let sink_impl: Arc<dyn ConnectionSink> = Arc::new(GatewaySink {
            event_tx: event_tx.clone(),
            frame_tx: frame_tx.clone(),
        });
        let connection = Arc::new(Connection::new(sink_impl));
        channel.attach(Arc::clone(&connection));

        // Drop our local reference to `event_tx` so the translator
        // task can exit cleanly once the channel detaches the
        // connection (which drops the sink's `event_tx`).
        drop(event_tx);

        let pump = tokio::spawn(pump_loop(sink, frame_rx));

        Self {
            channel,
            connection,
            frame_tx,
            pump,
        }
    }

    /// Push a frame directly to the outbound pump. Used for
    /// `RegisterAck` echoes during the handshake and other control
    /// frames the route layer authors itself.
    pub(crate) async fn send_frame(&self, frame: Frame) -> Result<(), ChannelError> {
        self.frame_tx
            .send(frame)
            .await
            .map_err(|_| ChannelError::Config("outbound pump closed".into()))
    }

    /// Clone the outbound frame sender. Used by the channel-control
    /// registry so the admin surface can push control frames
    /// (`StartBot` / `StopBot` / etc.) from outside the route task.
    pub(crate) fn frame_tx_clone(&self) -> mpsc::Sender<Frame> {
        self.frame_tx.clone()
    }

    /// Connection id assigned by the channel at attach time. Useful for
    /// logs and for the route layer's per-connection bookkeeping.
    pub(crate) fn connection_id(&self) -> ConnectionId {
        self.connection.id()
    }

    /// Resolve a pending approval and broadcast `ApprovalResolved` to
    /// every subscriber of the call's session. Called from the inbound
    /// loop when the client sends `ResolveApproval`. Returns `true` if
    /// a pending entry matched the call id. The session id is read off
    /// the resolved queue entry — the connection-side frame doesn't
    /// carry one, and dispatching with an empty id on a
    /// [`ChannelKind::Subscribed`] channel would silently fan out to
    /// nobody.
    pub(crate) fn resolve_approval(&self, call_id: &str, decision: ApprovalDecision) -> bool {
        let Some(session_id) = self.channel.resolve_approval(call_id, decision) else {
            return false;
        };
        super::boot::broadcast_approval_resolved(
            &self.channel,
            call_id.to_owned(),
            session_id,
            decision,
        );
        true
    }

    /// Detach the connection from its channel and return the pump
    /// join handle so the caller can await its shutdown. Idempotent on
    /// multiple calls because detach is best-effort.
    pub(crate) fn into_pump(self) -> JoinHandle<()> {
        let conn_id = self.connection.id();
        self.channel.detach(conn_id);
        drop(self.connection);
        drop(self.frame_tx);
        self.pump
    }
}

struct GatewaySink {
    event_tx: mpsc::Sender<SessionEvent>,
    frame_tx: mpsc::Sender<Frame>,
}

impl ConnectionSink for GatewaySink {
    fn try_send_event(&self, event: SessionEvent) -> SendOutcome {
        match self.event_tx.try_send(event) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => SendOutcome::Full,
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }

    fn try_send_frame(&self, frame: Frame) -> SendOutcome {
        match self.frame_tx.try_send(frame) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => SendOutcome::Full,
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }
}

async fn translator_loop(
    mut event_rx: mpsc::Receiver<SessionEvent>,
    frame_tx: mpsc::Sender<Frame>,
    channel_type: ChannelType,
    blob_store: Arc<dyn BlobStore>,
) {
    while let Some(event) = event_rx.recv().await {
        let frame = session_event_to_frame(event, &channel_type, blob_store.as_ref()).await;
        if frame_tx.send(frame).await.is_err() {
            break;
        }
    }
}

/// Server-initiated keepalive cadence. The web client force-closes a WS
/// it hasn't heard from in ~45s (its half-open watchdog), and a
/// backgrounded tab's own ping timer gets throttled by the browser — so
/// the *server* must emit periodic traffic to keep that watchdog fed.
/// Each `Ping` also draws a client `Pong`; either frame resets the
/// client's `lastFrameAt`. Comfortably under the client's liveness budget
/// so a single dropped frame doesn't trip it.
const KEEPALIVE_PING_INTERVAL: Duration = Duration::from_secs(20);

async fn pump_loop(mut sink: WsSink, mut frame_rx: mpsc::Receiver<Frame>) {
    // First tick fires one interval out, not immediately, so a chatty
    // connection never carries a redundant startup Ping.
    let mut keepalive = tokio::time::interval_at(
        tokio::time::Instant::now() + KEEPALIVE_PING_INTERVAL,
        KEEPALIVE_PING_INTERVAL,
    );
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let frame = tokio::select! {
            recv = frame_rx.recv() => match recv {
                Some(frame) => frame,
                None => break,
            },
            _ = keepalive.tick() => Frame::Ping,
        };
        let bytes = match wire::encode(&frame) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "encode outbound frame");
                continue;
            }
        };
        if let Err(e) = sink.send(AxumWsMessage::Binary(bytes.into())).await {
            tracing::debug!(error = %e, "ws sink error; pump exiting");
            break;
        }
    }
    let _ = sink.close().await;
}

async fn session_event_to_frame(
    event: SessionEvent,
    channel_type: &ChannelType,
    blob_store: &dyn BlobStore,
) -> Frame {
    match event {
        SessionEvent::Agent(output) => {
            agent_output_to_frame(output, channel_type, blob_store).await
        }
        SessionEvent::UserEcho(incoming) => {
            let (content, attachments) = split_content(&incoming.message.content, blob_store).await;
            Frame::Message(WireMessage {
                content,
                session_id: incoming.message.session_id,
                user_id: incoming.message.sender.id,
                channel_type: channel_type.clone(),
                bot_id: String::new(),
                attachments,
                // Carry the client's idempotency key back so the sender's
                // tab can reconcile its optimistic placeholder against the
                // echoed row by id instead of producing a duplicate.
                platform_msg_id: incoming.platform_msg_id,
                role: MessageRole::User,
                ordinal: None,
            })
        }
        SessionEvent::ApprovalRequested {
            call_id,
            session_id,
            user_id,
            tool,
            accesses,
            params_preview,
            description,
        } => Frame::ApprovalRequested {
            call_id,
            session_id,
            user_id,
            tool,
            accesses,
            params_preview,
            description,
        },
        SessionEvent::ApprovalResolved {
            call_id, decision, ..
        } => Frame::ApprovalResolved { call_id, decision },
    }
}

async fn agent_output_to_frame(
    output: AgentOutput,
    channel_type: &ChannelType,
    blob_store: &dyn BlobStore,
) -> Frame {
    let AgentOutput {
        session_id,
        user_id,
        event,
        ..
    } = output;
    match event {
        AgentEvent::AnswerDelta(text) => Frame::AnswerDelta {
            session_id,
            user_id,
            text,
        },
        AgentEvent::Reasoning(text) => Frame::Reasoning {
            session_id,
            user_id,
            text,
        },
        AgentEvent::ToolStarted {
            call_id,
            tool,
            label,
        } => Frame::ToolStarted {
            session_id,
            user_id,
            call_id,
            tool,
            label,
        },
        AgentEvent::ToolCompleted {
            call_id,
            status,
            summary,
        } => {
            let status = match status {
                ToolStatus::Ok => "ok",
                ToolStatus::Error => "error",
                ToolStatus::Denied => "denied",
            };
            Frame::ToolCompleted {
                session_id,
                user_id,
                call_id,
                status: status.to_owned(),
                summary,
            }
        }
        AgentEvent::Status(status) => {
            let phase = match status {
                TurnStatus::Compacting => "compacting",
                TurnStatus::Compacted => "compacted",
            };
            Frame::Status {
                session_id,
                user_id,
                phase: phase.to_owned(),
            }
        }
        AgentEvent::Message(response) => {
            let (content, attachments) = split_content(&response.content, blob_store).await;
            Frame::Message(WireMessage {
                content,
                session_id: response.session_id,
                // Populate with the addressee so sidecars can route by
                // user (Telegram: `user_id → chat_id`) without having to
                // maintain a `session_id → user_id` reverse map.
                user_id: response.user_id,
                channel_type: channel_type.clone(),
                bot_id: String::new(),
                attachments,
                platform_msg_id: String::new(),
                role: MessageRole::Assistant,
                // Carry the persisted assistant ordinal so the client's
                // reconnect cursor can advance past this row. See
                // `OutgoingMessage::ordinal` for why it can be `None`.
                ordinal: response.ordinal,
            })
        }
        AgentEvent::Notice { level, text } => {
            let level = match level {
                NoticeLevel::Info => "info",
                NoticeLevel::Warn => "warn",
                NoticeLevel::Error => "error",
            };
            Frame::Notice {
                session_id,
                user_id,
                level: level.to_owned(),
                text,
                transient: false,
            }
        }
        // Transient progress narration (the observer) rides the same
        // notice frame so banner-only surfaces (TUI, sidecars) render it
        // exactly like an info notice; the `transient` flag tells a
        // work-block client (web) to fold it into the open turn instead
        // of collapsing it.
        AgentEvent::Progress(text) => Frame::Notice {
            session_id,
            user_id,
            level: "info".to_owned(),
            text,
            transient: true,
        },
        AgentEvent::TaskList(tasks) => Frame::TaskList {
            session_id,
            user_id,
            tasks: tasks.into_iter().map(TaskView::from).collect(),
        },
        AgentEvent::TurnState { active, started_at } => Frame::TurnState {
            session_id,
            user_id,
            active,
            started_at,
        },
        // Reuse `split_content`'s media→`WireAttachment` mapping; the
        // text half is empty for the media-only blocks this carries.
        AgentEvent::Attachment(blocks) => {
            let (_text, attachments) = split_content(&blocks, blob_store).await;
            Frame::Attachment {
                session_id,
                user_id,
                attachments,
            }
        }
    }
}

/// Walk the agent's content blocks and split them into the wire's
/// `(text, attachments)` shape. Text blocks fold into a single newline-
/// joined string; media blocks become `WireAttachment` entries with
/// metadata pulled from the blob store. A blob whose `stat` fails is
/// dropped from the outbound — the agent's intent of "send this media"
/// can't be honored without a known mime/size, and surfacing a partial
/// payload would mislead the sidecar (and ultimately the user).
pub(super) async fn split_content(
    blocks: &[ContentBlock],
    blob_store: &dyn BlobStore,
) -> (String, Vec<WireAttachment>) {
    let mut text = String::new();
    let mut attachments = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::Image { blob, mime_type } => {
                if let Some(att) = stat_attachment(
                    blob_store,
                    AttachmentKind::Image,
                    &blob.blob_id,
                    Some(mime_type.clone()),
                    None,
                )
                .await
                {
                    attachments.push(att);
                }
            }
            ContentBlock::Audio { blob, mime_type } => {
                if let Some(att) = stat_attachment(
                    blob_store,
                    AttachmentKind::Audio,
                    &blob.blob_id,
                    Some(mime_type.clone()),
                    None,
                )
                .await
                {
                    attachments.push(att);
                }
            }
            ContentBlock::File {
                blob,
                filename,
                mime_type,
            } => {
                if let Some(att) = stat_attachment(
                    blob_store,
                    AttachmentKind::File,
                    &blob.blob_id,
                    Some(mime_type.clone()),
                    Some(filename.clone()),
                )
                .await
                {
                    attachments.push(att);
                }
            }
            // ToolUse / ToolResult / Thinking are agent-internal and
            // never propagate to channel-facing frames.
            ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Thinking { .. } => {}
        }
    }
    (text, attachments)
}

async fn stat_attachment(
    blob_store: &dyn BlobStore,
    kind: AttachmentKind,
    blob_id: &str,
    mime_override: Option<String>,
    filename: Option<String>,
) -> Option<WireAttachment> {
    match blob_store.stat(blob_id).await {
        Ok(meta) => {
            let size = match u32::try_from(meta.size) {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(
                        blob_id = %meta.blob_id,
                        size = meta.size,
                        "attachment exceeds u32 size cap; dropping",
                    );
                    return None;
                }
            };
            Some(WireAttachment {
                kind,
                blob_id: meta.blob_id,
                mime_type: mime_override.unwrap_or(meta.mime_type),
                size,
                filename,
            })
        }
        Err(e) => {
            tracing::warn!(blob_id, error = %e, "attachment blob stat failed; dropping");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_or_install_channel;
    use aura_channels::wire::Frame;
    use aura_channels::{AgentEvent, AgentOutput, ChannelRegistry, NoticeLevel};
    use aura_model::ChannelType;
    use aura_storage::test_support::MemoryBlobStore;
    use std::sync::Arc;

    /// An installed channel is returned directly without invoking the
    /// lazy install path. Guards the order: we look up *first*, fall
    /// back to install only on miss — flipping the order would
    /// double-install on every reconnect and return DuplicateChannel.
    #[test]
    fn resolve_returns_pre_installed_channel() {
        let registry = Arc::new(ChannelRegistry::new());
        super::super::boot::install_channel(&registry, ChannelType::http()).expect("install");
        let before = registry.get(&ChannelType::http()).expect("pre-installed");

        let resolved =
            resolve_or_install_channel(&registry, &ChannelType::http()).expect("resolve");
        assert!(
            Arc::ptr_eq(&before, &resolved),
            "resolve must return the existing Arc, not a freshly-installed sibling",
        );
    }

    /// An unknown channel type triggers the lazy install fallback so
    /// out-of-tree sidecars declared via `aura.json` (not in the
    /// built-in `install_channels` map) still get a registry slot
    /// when their first connection lands.
    #[test]
    fn resolve_lazy_installs_unknown_channel() {
        let registry = Arc::new(ChannelRegistry::new());
        let ct = ChannelType::from("custom-out-of-tree");
        assert!(registry.get(&ct).is_none(), "fixture must start empty");

        let resolved = resolve_or_install_channel(&registry, &ct).expect("lazy install");
        assert_eq!(resolved.channel_type().as_str(), "custom-out-of-tree");
        assert!(
            registry.get(&ct).is_some(),
            "lazy install must publish to the registry so a second connect hits the hot path",
        );
    }

    /// The progress observer's `Progress` rides the notice frame so
    /// banner-only surfaces render it like any info notice, but carries
    /// `transient: true` so a work-block client folds it into the open
    /// turn instead of collapsing it (the "two `Worked Xs`" bug).
    #[tokio::test]
    async fn progress_event_maps_to_transient_info_notice() {
        let out = AgentOutput {
            session_id: "s1".into(),
            user_id: "u1".to_owned(),
            channel: ChannelType::http(),
            event: AgentEvent::Progress("still working on it".to_owned()),
        };
        let blobs = MemoryBlobStore::new();
        let frame = super::agent_output_to_frame(out, &ChannelType::http(), &blobs).await;
        match frame {
            Frame::Notice {
                level,
                text,
                transient,
                ..
            } => {
                assert_eq!(level, "info");
                assert_eq!(text, "still working on it");
                assert!(transient, "progress must be transient");
            }
            other => panic!("expected a notice frame, got {other:?}"),
        }
    }

    /// A genuine (terminal) notice maps to `transient: false` so it stays
    /// the turn's reply and downstream clients see byte-identical frames.
    #[tokio::test]
    async fn notice_event_is_not_transient() {
        let out = AgentOutput {
            session_id: "s1".into(),
            user_id: "u1".to_owned(),
            channel: ChannelType::http(),
            event: AgentEvent::Notice {
                level: NoticeLevel::Warn,
                text: "heads up".to_owned(),
            },
        };
        let blobs = MemoryBlobStore::new();
        let frame = super::agent_output_to_frame(out, &ChannelType::http(), &blobs).await;
        match frame {
            Frame::Notice {
                level, transient, ..
            } => {
                assert_eq!(level, "warn");
                assert!(!transient, "a terminal notice must not be transient");
            }
            other => panic!("expected a notice frame, got {other:?}"),
        }
    }
}
