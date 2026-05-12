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

use aura_channels::wire::{self, AttachmentKind, Frame, Message as WireMessage, WireAttachment};
use aura_channels::{
    AgentOutput, Channel, ChannelError, ChannelRegistry, Connection, ConnectionId, ConnectionSink,
    MessageRole, NoticeLevel, SendOutcome, SessionEvent,
};
use aura_model::{ChannelType, ContentBlock, SessionId};
use aura_storage::BlobStore;
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

impl Sidecar {
    /// Build the per-WS state. Looks up the channel in the registry;
    /// if absent (e.g. test fixtures that skipped boot install), the
    /// channel is created on the fly via [`super::boot::install_channel`]
    /// so production and tests follow the same install path.
    pub(crate) fn build(
        channel_type: ChannelType,
        registry: &Arc<ChannelRegistry>,
        sink: WsSink,
        blob_store: Arc<dyn BlobStore>,
    ) -> Result<Self, ChannelError> {
        let channel = match registry.get(&channel_type) {
            Some(ch) => ch,
            None => {
                super::boot::install_channel(registry, channel_type.clone())?;
                registry.get(&channel_type).ok_or_else(|| {
                    ChannelError::Config(format!("channel '{channel_type}' missing after install"))
                })?
            }
        };

        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(OUTBOUND_BUFFER);
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(OUTBOUND_BUFFER);

        // Translator: SessionEvent → Frame. Exits when every clone of
        // `event_tx` drops (channel detach + sidecar drop).
        let translator_tx = frame_tx.clone();
        let translator_ct = channel_type.clone();
        let translator_blobs = Arc::clone(&blob_store);
        tokio::spawn(translator_loop(
            event_rx,
            translator_tx,
            translator_ct,
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

        Ok(Self {
            channel,
            connection,
            frame_tx,
            pump,
        })
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
    /// a pending entry matched the call id.
    pub(crate) fn resolve_approval(
        &self,
        call_id: &str,
        session_id: &SessionId,
        decision: ApprovalDecision,
    ) -> bool {
        let resolved = self.channel.resolve_approval(call_id, decision);
        if resolved {
            super::boot::broadcast_approval_resolved(
                &self.channel,
                call_id.to_owned(),
                session_id.clone(),
                decision,
            );
        }
        resolved
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

async fn pump_loop(mut sink: WsSink, mut frame_rx: mpsc::Receiver<Frame>) {
    while let Some(frame) = frame_rx.recv().await {
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
    match output {
        AgentOutput::Delta {
            session_id,
            user_id,
            text,
            ..
        } => Frame::Delta {
            session_id,
            user_id,
            text,
        },
        AgentOutput::Message(response) => {
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
                ordinal: None,
            })
        }
        AgentOutput::Notice {
            session_id,
            user_id,
            level,
            text,
            ..
        } => {
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
async fn split_content(
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
