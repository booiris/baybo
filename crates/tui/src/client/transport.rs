//! WebSocket + MessagePack-backed TUI transport.
//!
//! Wraps [`WsClient`] and a local [`ApprovalQueue`]. User input is
//! sent as [`wire::Message`] frames; streaming deltas, final responses,
//! notices, and approval events flow back as typed [`wire::Frame`]s
//! and are mapped onto the TUI's [`TransportEvent`].
//!
//! Outbound approvals — driven by the TUI's modal — travel over the
//! same socket as [`wire::Frame::ResolveApproval`] so the gateway's
//! per-connection approval gate releases the pending tool call.

use std::net::SocketAddr;
use std::sync::Arc;

use baybo_channels::wire::{Frame, Message as WireMessage};
use baybo_channels::{ChannelError, IncomingMessage, MessageRole, NoticeLevel, Result};
use baybo_model::{ChannelType, ContentBlock, SessionId};
use baybo_tools::{ApprovalQueue, ApprovalRequest};
use parking_lot::RwLock;
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, warn};

use super::ws::{WsClient, WsClientError};
use crate::transport::{TransportEvent, TransportEventStream};

/// Channel capacity for the subscribe pump. Deep enough to absorb a
/// burst of delta frames between redraws; shallow enough that the
/// receiver side backpressures the pump if the TUI stalls.
const EVENT_CHAN_CAPACITY: usize = 64;

/// WS-backed TUI transport. Owns a single live [`WsClient`] per TUI
/// process — the TUI multiplexes all sessions over it.
pub struct WsTransport {
    client: Arc<WsClient>,
    approval_queue: ApprovalQueue,
    // Hold subscribe()'s pump handles so a second subscribe on a new
    // session doesn't race the previous one on the shared source.
    subscribe_lock: Arc<Mutex<()>>,
    /// Pinned session this connection is currently bound to. Shared
    /// with the subscribe pump (which reads it on every inbound frame
    /// to drop messages that target the old session after a `/new`
    /// switch) and rewritten by [`WsTransport::switch_session`]. Held
    /// behind a `parking_lot::RwLock` because rewrites are rare (only
    /// on `/new`) and reads are tiny / hot — uncontended `read()` is
    /// a load+CAS.
    session_pin: Arc<RwLock<SessionId>>,
    /// One-shot history ring the gateway pushed right after
    /// `RegisterAck`. The TUI consumes it once during bootstrap — see
    /// [`WsTransport::take_history_snapshot`].
    initial_history: Mutex<Option<Vec<String>>>,
}

impl WsTransport {
    /// Dial the gateway's admin TCP listener (which co-hosts
    /// `/v1/channel-ws`) with the TUI vault-token and register as the
    /// built-in `"tui"` channel. `session_id` pins this TUI instance
    /// to one session so multiple concurrent TUI processes can share
    /// the same gateway — the gateway routes events for that session
    /// to this connection only.
    pub async fn connect(
        addr: SocketAddr,
        tui_token: String,
        session_id: SessionId,
    ) -> Result<Self> {
        let (client, initial_history) = WsClient::connect_tui(
            addr,
            &tui_token,
            ChannelType::from("tui"),
            session_id.clone(),
        )
        .await
        .map_err(|e| match e {
            // Only true "nothing's listening on the port" failures
            // should trigger the auto-spawn gateway path upstream.
            // Handshake / protocol errors mean a gateway is alive
            // and we should surface the real reason instead.
            WsClientError::TcpDial(_) => ChannelError::NotReachable(format!("tui ws connect: {e}")),
            _ => ChannelError::Config(format!("tui ws connect: {e}")),
        })?;
        let client = Arc::new(client);

        let approval_queue = ApprovalQueue::new();
        // Local resolver: echo the modal's decision back to the gateway
        // as a ResolveApproval frame so the tool gate releases
        // server-side.
        let client_for_resolver: Arc<WsClient> = Arc::clone(&client);
        approval_queue.set_resolver(Arc::new(move |call_id, decision| {
            let client = Arc::clone(&client_for_resolver);
            tokio::spawn(async move {
                let frame = Frame::ResolveApproval {
                    call_id: call_id.clone(),
                    decision,
                };
                if let Err(e) = client.send_raw(&frame).await {
                    warn!(call_id, "approval resolution failed: {e}");
                }
            });
        }));

        Ok(Self {
            client,
            approval_queue,
            subscribe_lock: Arc::new(Mutex::new(())),
            session_pin: Arc::new(RwLock::new(session_id)),
            initial_history: Mutex::new(Some(initial_history)),
        })
    }
}

impl WsTransport {
    /// Hand an incoming user message off to the gateway.
    pub async fn submit(&self, msg: IncomingMessage) -> Result<()> {
        let text = msg
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let wire_msg = WireMessage {
            content: text,
            session_id: msg.message.session_id,
            user_id: msg.message.sender.id,
            channel_type: msg.message.channel,
            // TUI subscribes to its own session and bypasses the
            // bot-pairing gate.
            bot_id: String::new(),
            // The TUI is text-only on the input side; media attachments
            // arrive via the agent and are rendered by the chat view's
            // own placeholder logic.
            attachments: Vec::new(),
            // TUI is selective-kind — dedup is not relevant.
            platform_msg_id: String::new(),
            role: baybo_channels::MessageRole::User,
            // Inbound to gateway — server picks up the ordinal on
            // persistence; outbound frames never need it set here.
            ordinal: None,
        };

        self.client
            .send(wire_msg)
            .await
            .map_err(|e| ChannelError::Send(format!("tui ws send: {e}")))
    }

    /// Open a long-lived subscription to gateway events for the
    /// session this transport is currently pinned to. The returned
    /// stream ends when the peer closes. The pump re-reads the pin on
    /// every inbound frame so a [`WsTransport::switch_session`] call
    /// immediately rolls over to the new id without a stream restart.
    pub async fn subscribe(&self) -> Result<TransportEventStream> {
        let (tx, rx) = mpsc::channel::<Result<TransportEvent>>(EVENT_CHAN_CAPACITY);
        let client: Arc<WsClient> = Arc::clone(&self.client);
        let queue = self.approval_queue.clone();
        let target = Arc::clone(&self.session_pin);
        let subscribe_lock = Arc::clone(&self.subscribe_lock);

        tokio::spawn(async move {
            // Serialize pump tasks so a second subscribe (e.g. on
            // session switch) doesn't concurrently drain the shared
            // WS source.
            let _guard = subscribe_lock.lock().await;
            loop {
                let frame = match client.recv_any().await {
                    Ok(f) => f,
                    Err(WsClientError::PeerClosed) => {
                        debug!("tui ws peer closed; subscribe pump exiting");
                        return;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(ChannelError::Send(format!("tui ws recv: {e}"))))
                            .await;
                        return;
                    }
                };
                let target_snapshot = target.read().clone();
                if let Some(event) = map_frame(frame, &target_snapshot, &queue)
                    && tx.send(Ok(event)).await.is_err()
                {
                    return;
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)) as TransportEventStream)
    }

    /// Current session id this transport is pinned to. Cloned out of
    /// the shared pin; callers needing to compare or render the id
    /// should call this once per use rather than caching, so a
    /// [`Self::switch_session`] elsewhere stays visible.
    pub fn current_session_id(&self) -> SessionId {
        self.session_pin.read().clone()
    }

    /// Abandon the current session and start a fresh one. Sends an
    /// `Unsubscribe` for the old id and a `Subscribe` for `new_id` on
    /// the existing WS, then rewrites the shared pin so the subscribe
    /// pump and `append_history` immediately target the new session.
    /// No new gateway round-trip is needed beyond the two control
    /// frames: the session row itself is created lazily on the agent
    /// side when the first user message lands (see
    /// `SessionManager::get_or_create`), so a `/new` that ends without
    /// a message leaves no trace in the session store.
    pub async fn switch_session(&self, new_id: SessionId) -> Result<()> {
        let old_id = self.session_pin.read().clone();
        if old_id == new_id {
            return Ok(());
        }
        self.client
            .send_raw(&Frame::Unsubscribe {
                session_id: old_id.clone(),
            })
            .await
            .map_err(|e| ChannelError::Send(format!("tui ws unsubscribe: {e}")))?;
        self.client
            .send_raw(&Frame::Subscribe {
                session_id: new_id.clone(),
                since_ordinal: None,
            })
            .await
            .map_err(|e| ChannelError::Send(format!("tui ws subscribe: {e}")))?;
        *self.session_pin.write() = new_id;
        Ok(())
    }

    /// Shared approval queue. The TUI renders pending entries from it
    /// and the modal resolver echoes decisions back as
    /// [`Frame::ResolveApproval`].
    pub fn approval_queue(&self) -> ApprovalQueue {
        self.approval_queue.clone()
    }

    /// Consume the one-shot history ring the gateway pushed right after
    /// register. Returns `None` on subsequent calls — the ring is meant
    /// to be consumed once during TUI bootstrap.
    pub async fn take_history_snapshot(&self) -> Option<Vec<String>> {
        self.initial_history.lock().await.take()
    }

    /// Fire-and-forget: persist one submitted input line on the
    /// gateway-side history store. The server does not ack per-append
    /// — any send failure is surfaced as a returned error so the caller
    /// can decide whether to log or retry.
    pub async fn append_history(&self, entry: &str) -> Result<()> {
        let frame = Frame::HistoryAppend {
            session_id: self.current_session_id(),
            entry: entry.to_owned(),
        };
        self.client
            .send_raw(&frame)
            .await
            .map_err(|e| ChannelError::Send(format!("tui ws history append: {e}")))
    }
}

fn map_frame(
    frame: Frame,
    target_session: &SessionId,
    queue: &ApprovalQueue,
) -> Option<TransportEvent> {
    match frame {
        Frame::AnswerDelta {
            session_id, text, ..
        } => {
            if &session_id != target_session {
                return None;
            }
            Some(TransportEvent::StreamDelta(text))
        }
        Frame::Message(msg) => {
            if &msg.session_id != target_session {
                return None;
            }
            // `tui` is a `Subscribed` channel, so the gateway echoes
            // inbound user messages back to every subscriber — including
            // the sender. The TUI already rendered the prompt locally
            // via `state.push_user` before dispatch, so the echo would
            // duplicate the row (and render with the wrong role). Drop
            // user echoes; only assistant rows are agent output we
            // haven't already shown.
            if matches!(msg.role, MessageRole::User) {
                return None;
            }
            Some(TransportEvent::Response(vec![ContentBlock::Text(
                msg.content,
            )]))
        }
        // Client → server only (the web "send every queued message at once"
        // batch); never arrives inbound.
        Frame::Messages { .. } => None,
        Frame::Notice {
            session_id,
            level,
            text,
            ..
        } => {
            if &session_id != target_session {
                return None;
            }
            let level = match level.as_str() {
                "error" => NoticeLevel::Error,
                "info" => NoticeLevel::Info,
                _ => NoticeLevel::Warn,
            };
            Some(TransportEvent::Notice { level, text })
        }
        Frame::ApprovalRequested {
            call_id,
            session_id,
            user_id,
            tool,
            accesses,
            params_preview,
            description,
        } => {
            if &session_id != target_session {
                debug!(
                    call_id,
                    tool,
                    other = %session_id,
                    "approval from other session; ignoring"
                );
                return None;
            }
            debug!(call_id, tool, "approval added from gateway");
            queue.enqueue_mirror(ApprovalRequest {
                call_id,
                session_id,
                user_id,
                tool,
                accesses,
                params_preview,
                description,
            });
            Some(TransportEvent::ApprovalRequested)
        }
        Frame::ApprovalResolved { call_id, decision } => {
            let _ = queue.drop_call(&call_id);
            Some(TransportEvent::ApprovalResolved { call_id, decision })
        }
        Frame::Reasoning { .. } => {
            // The TUI surfaces an animated "working" indicator instead of
            // the model's reasoning trace, so reasoning frames are dropped
            // here regardless of session. Other channels (e.g. the web UI)
            // still render them off the same wire frame.
            None
        }
        Frame::TaskList { .. } => {
            // The TUI has no live-checklist panel; the planning list is
            // surfaced on the web dashboard. Drop it here like Reasoning.
            None
        }
        Frame::TurnState { .. } => {
            // The TUI tracks turn activity locally (it initiated the turn),
            // so the server-side snapshot is redundant here. Drop it like
            // Reasoning; the web dashboard is its consumer.
            None
        }
        Frame::WorkSnapshot { .. } => {
            // The in-flight work-block snapshot recovers the reasoning/tool
            // steps a reconnecting work-block client missed. The TUI drops
            // Reasoning already, so it has nothing to render here either.
            None
        }
        Frame::ToolStarted {
            session_id,
            call_id,
            tool,
            label,
            ..
        } => {
            if &session_id != target_session {
                return None;
            }
            Some(TransportEvent::ToolStarted {
                call_id,
                tool,
                label,
            })
        }
        Frame::ToolCompleted {
            session_id,
            call_id,
            status,
            summary,
            ..
        } => {
            if &session_id != target_session {
                return None;
            }
            Some(TransportEvent::ToolCompleted {
                call_id,
                status,
                summary,
            })
        }
        Frame::Status {
            session_id, phase, ..
        } => {
            if &session_id != target_session {
                return None;
            }
            Some(TransportEvent::Status { phase })
        }
        Frame::SessionUpdated { .. }
        | Frame::SessionActivity { .. }
        | Frame::FoldersChanged { .. } => {
            // Web-chat sidebar signals — TUI tracks a single session
            // of its own and has no list view, so it ignores rather
            // than warning.
            None
        }
        Frame::PendingApprovalsSnapshot { .. } => {
            // Web reconciles stale ApprovalCard state against this;
            // the TUI's `ApprovalQueue` is already keyed by the live
            // `ApprovalRequested` / `ApprovalResolved` fan-out and
            // doesn't keep cards across disconnects, so the snapshot
            // is redundant here.
            None
        }
        Frame::Ping | Frame::Pong => {
            // The TUI doesn't currently drive the heartbeat itself —
            // its tokio task is already idle-aware. Accept either
            // direction silently so a future server-initiated probe
            // doesn't have to negotiate per-client.
            None
        }
        Frame::HistorySnapshot { .. } => {
            // First HistorySnapshot is drained by `connect_tui`; the
            // gateway also resends one on every `Subscribe` (including
            // the `/new`-driven re-subscribe), so a stray one
            // post-handshake is expected, not a violation. Input
            // history is conceptually global to the TUI process, not
            // per-session, so the resends carry no useful new state
            // and are silently dropped.
            None
        }
        Frame::Attachment { .. } => {
            // The TUI can't render media, so drop the standalone
            // attachment frame. Deliberately NOT turned into a
            // `TransportEvent` — it carries no turn-completion meaning and
            // must not disturb the spinner / outstanding-response state.
            None
        }
        Frame::Register { .. }
        | Frame::RegisterAck { .. }
        | Frame::Subscribe { .. }
        | Frame::Unsubscribe { .. }
        | Frame::Reset { .. }
        | Frame::ResolveApproval { .. }
        | Frame::HistoryAppend { .. }
        | Frame::StartBot { .. }
        | Frame::StopBot { .. }
        | Frame::BotStatus { .. }
        | Frame::SlashManifest { .. } => {
            // StartBot / StopBot / BotStatus are sidecar control-plane
            // frames — the TUI never participates in that flow.
            // SlashManifest is also sidecar-only: the TUI owns its
            // slash commands client-side via TuiSlashHandler.
            warn!("unexpected frame from gateway; dropping");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::from(s)
    }

    #[test]
    fn map_frame_forwards_delta_for_pinned_session() {
        let queue = ApprovalQueue::new();
        let target = sid("alpha");
        let frame = Frame::AnswerDelta {
            session_id: target.clone(),
            user_id: String::new(),
            text: "tick".into(),
        };
        match map_frame(frame, &target, &queue) {
            Some(TransportEvent::StreamDelta(text)) => assert_eq!(text, "tick"),
            other => panic!("expected StreamDelta, got {other:?}"),
        }
    }

    #[test]
    fn map_frame_drops_delta_for_other_session() {
        // Models the in-flight tail after `/new`: gateway emits a few
        // more Deltas for the old session id before our Unsubscribe
        // reaches it; the pump's filter must drop them rather than
        // leak old-session output into the new transcript.
        let queue = ApprovalQueue::new();
        let target = sid("beta");
        let frame = Frame::AnswerDelta {
            session_id: sid("alpha"),
            user_id: String::new(),
            text: "stale".into(),
        };
        assert!(map_frame(frame, &target, &queue).is_none());
    }

    #[test]
    fn map_frame_drops_history_snapshot_silently() {
        // Subscribe always triggers a HistorySnapshot from the gateway
        // (including the `/new`-driven re-subscribe). Input history is
        // global to the TUI process, so the resend carries nothing the
        // pump cares about — it must be dropped, not warned about.
        let queue = ApprovalQueue::new();
        let target = sid("alpha");
        let frame = Frame::HistorySnapshot {
            session_id: target.clone(),
            entries: vec!["echo".into()],
        };
        assert!(map_frame(frame, &target, &queue).is_none());
    }

    #[test]
    fn map_frame_forwards_tool_lifecycle_for_pinned_session() {
        let queue = ApprovalQueue::new();
        let target = sid("alpha");
        match map_frame(
            Frame::ToolStarted {
                session_id: target.clone(),
                user_id: String::new(),
                call_id: "c1".into(),
                tool: "Read".into(),
                label: Some("foo.rs".into()),
            },
            &target,
            &queue,
        ) {
            Some(TransportEvent::ToolStarted {
                call_id,
                tool,
                label,
            }) => {
                assert_eq!(call_id, "c1");
                assert_eq!(tool, "Read");
                assert_eq!(label.as_deref(), Some("foo.rs"));
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        match map_frame(
            Frame::ToolCompleted {
                session_id: target.clone(),
                user_id: String::new(),
                call_id: "c1".into(),
                status: "ok".into(),
                summary: "200 lines".into(),
            },
            &target,
            &queue,
        ) {
            Some(TransportEvent::ToolCompleted {
                call_id,
                status,
                summary,
            }) => {
                assert_eq!(call_id, "c1");
                assert_eq!(status, "ok");
                assert_eq!(summary, "200 lines");
            }
            other => panic!("expected ToolCompleted, got {other:?}"),
        }
    }

    #[test]
    fn map_frame_drops_reasoning_even_for_pinned_session() {
        // The TUI renders an animated working indicator instead of the
        // reasoning trace, so a Reasoning frame is dropped unconditionally
        // — including one addressed to the pinned session.
        let queue = ApprovalQueue::new();
        let target = sid("alpha");
        let frame = Frame::Reasoning {
            session_id: target.clone(),
            user_id: String::new(),
            text: "a thought".into(),
        };
        assert!(map_frame(frame, &target, &queue).is_none());
    }

    #[test]
    fn map_frame_drops_user_echo_for_pinned_session() {
        // Subscribed-kind channels echo inbound to every subscriber —
        // including the sender. The TUI already rendered its own
        // prompt locally, so the echo must not duplicate the row.
        let queue = ApprovalQueue::new();
        let target = sid("alpha");
        let echo = Frame::Message(WireMessage {
            content: "hi".into(),
            session_id: target.clone(),
            user_id: "u".into(),
            channel_type: ChannelType::tui(),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::User,
            ordinal: None,
        });
        assert!(map_frame(echo, &target, &queue).is_none());
    }
}
