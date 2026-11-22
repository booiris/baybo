//! WebSocket + MessagePack-backed TUI transport.
//!
//! Wraps [`WsClient`] and the client's [`PendingAsks`]. User input is
//! sent as [`wire::Message`](baybo_channels::wire::Message) frames; streaming deltas, final responses,
//! notices, approvals, and questions flow back as typed [`wire::Frame`](baybo_channels::wire::Frame)s
//! and are mapped onto the TUI's [`TransportEvent`].
//!
//! The subscribe pump sends both approval decisions and structured question replies.

use std::net::SocketAddr;
use std::sync::Arc;

use baybo_channels::wire::{Frame, Message as WireMessage};
use baybo_channels::{ChannelError, IncomingMessage, MessageRole, NoticeLevel, Result};
use baybo_model::{ChannelType, ContentBlock, SessionId};
use baybo_tools::ApprovalRequest;
use parking_lot::RwLock;
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, warn};

use super::asks::{PendingAsks, PendingCard};
use super::ws::{WsClient, WsClientError};
use crate::transport::{TransportEvent, TransportEventStream};

/// Channel capacity for the subscribe pump. Deep enough to absorb a
/// burst of delta frames between redraws; shallow enough that the
/// receiver side backpressures the pump if the TUI stalls.
const EVENT_CHAN_CAPACITY: usize = 64;

/// Upgrade statuses that mean "live gateway, credential refused" rather
/// than "broken handshake". They map to [`ChannelError::Unauthorized`]
/// so the CLI prints the stale-token remedy instead of telling the user
/// to start a gateway that is already running.
const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_FORBIDDEN: u16 = 403;

/// WS-backed TUI transport. Owns a single live [`WsClient`] per TUI
/// process — the TUI multiplexes all sessions over it.
pub struct WsTransport {
    client: Arc<WsClient>,
    asks: PendingAsks,
    // One pump owns both the outbound replies and the inbound WS stream.
    answers: Arc<Mutex<mpsc::UnboundedReceiver<Frame>>>,
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
            WsClientError::WsUpgradeRejected {
                status: HTTP_UNAUTHORIZED | HTTP_FORBIDDEN,
            } => ChannelError::Unauthorized(format!("tui ws connect: {e}")),
            _ => ChannelError::Config(format!("tui ws connect: {e}")),
        })?;
        let client = Arc::new(client);

        let (asks, answers) = PendingAsks::new();

        Ok(Self {
            client,
            asks,
            answers: Arc::new(Mutex::new(answers)),
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
        let asks = self.asks.clone();
        let target = Arc::clone(&self.session_pin);
        let answers = Arc::clone(&self.answers);

        tokio::spawn(async move {
            // Serialize pump tasks so a second subscribe (e.g. on
            // session switch) doesn't concurrently drain the shared
            // WS source.
            let mut answers = answers.lock().await;
            loop {
                let received = tokio::select! {
                    Some(answer) = answers.recv() => {
                        if let Err(e) = client.send_raw(&answer).await {
                            asks.send_failed(&answer);
                            let _ = tx.send(Err(ChannelError::Send(format!("tui ws answer: {e}")))).await;
                            let _ = tx.send(Ok(TransportEvent::AsksChanged)).await;
                        }
                        continue;
                    }
                    frame = client.recv_any() => frame,
                };
                let frame = match received {
                    Ok(f) => f,
                    Err(WsClientError::PeerClosed) => {
                        answers.close();
                        let _ = tx
                            .send(Ok(TransportEvent::Notice {
                                level: NoticeLevel::Warn,
                                text: "Gateway disconnected. Reopen the same session to continue."
                                    .into(),
                            }))
                            .await;
                        return;
                    }
                    Err(e) => {
                        answers.close();
                        let _ = tx
                            .send(Err(ChannelError::Send(format!("tui ws disconnected: {e}"))))
                            .await;
                        return;
                    }
                };
                if let Frame::Gap { session_id } = frame {
                    let pin = target.read().clone();
                    if session_id.is_none_or(|id| id == pin)
                        && let Err(e) = client.send_raw(&Frame::Subscribe { session_id: pin }).await
                    {
                        let _ = tx
                            .send(Err(ChannelError::Send(format!("tui ws resubscribe: {e}"))))
                            .await;
                    }
                    continue;
                }
                let event = map_frame(frame, &target.read(), &asks);
                if let Some(event) = event
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
    /// the existing WS, pinning the new session before subscribing so the
    /// pump accepts the first snapshot even if the gateway replies immediately.
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
        {
            let mut pin = self.session_pin.write();
            *pin = new_id.clone();
            self.asks.clear();
        }
        if let Err(e) = self
            .client
            .send_raw(&Frame::Subscribe { session_id: new_id })
            .await
        {
            *self.session_pin.write() = old_id;
            return Err(ChannelError::Send(format!("tui ws subscribe: {e}")));
        }
        Ok(())
    }

    /// The pending approvals and questions in gateway order. The TUI
    /// renders the head and replies over the same socket.
    pub fn asks(&self) -> PendingAsks {
        self.asks.clone()
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
    asks: &PendingAsks,
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
            ask_id,
            tool_call_id,
            session_id,
            user_id,
            tool,
            accesses,
            params_preview,
            description,
        } => {
            if &session_id != target_session {
                debug!(
                    %ask_id,
                    tool,
                    other = %session_id,
                    "approval from other session; ignoring"
                );
                return None;
            }
            debug!(%ask_id, tool, "approval added from gateway");
            asks.show(PendingCard::Approval(ApprovalRequest {
                ask_id,
                tool_call_id: tool_call_id.unwrap_or_default(),
                session_id,
                user_id,
                tool,
                accesses,
                params_preview,
                description,
            }));
            Some(TransportEvent::AsksChanged)
        }
        // Every close, answered elsewhere or not: drop the card. A no-op
        // when this client's own answer already removed it.
        Frame::AskResolved { ask_id, .. } => {
            asks.close(&ask_id);
            Some(TransportEvent::AsksChanged)
        }
        Frame::QuestionAsked { ask_id, session_id, questions, .. } => {
            if &session_id != target_session || questions.is_empty() {
                return None;
            }
            asks.show(PendingCard::question(ask_id, questions));
            Some(TransportEvent::AsksChanged)
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
        Frame::SubscribeState {
            session_id,
            pending_approvals,
            pending_questions,
            ..
        } => {
            if &session_id != target_session {
                return None;
            }
            asks.replace(&session_id, pending_approvals, pending_questions);
            Some(TransportEvent::AsksChanged)
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
        | Frame::FoldersChanged { .. }
        | Frame::DeckCardData { .. }
        | Frame::DeckChanged
        // Board traffic on the shared owner channel — expected, not a
        // protocol violation, and the TUI has no board to update.
        | Frame::ProjectChanged { .. } => {
            // Web-chat sidebar / deck signals — TUI tracks a single
            // session of its own and has no list or deck view, so it
            // ignores rather than warning.
            None
        }
        // The pump requests a fresh pending set on a gap.
        Frame::Gap { .. } => None,
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
        Frame::Register { .. }
        | Frame::RegisterAck { .. }
        | Frame::Subscribe { .. }
        | Frame::Unsubscribe { .. }
        | Frame::ResolveApproval { .. }
        | Frame::AnswerQuestion { .. }
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
    fn live_and_restored_questions_share_the_queue_and_only_the_displayed_id_can_answer() {
        use crate::app::AppState;
        use baybo_channels::wire::{
            ApprovalCard, AskOutcomeKind, QuestionCard, QuestionReply, TurnSnapshot,
        };
        use baybo_model::human_ask::Question;

        let (asks, mut answers) = PendingAsks::new();
        let mut state = AppState::with_asks(asks.clone());
        let session = sid("here");
        let q = QuestionCard {
            queue_index: 0,
            ask_id: "question".parse().unwrap(),
            tool_call_id: "call".into(),
            user_id: String::new(),
            questions: vec![Question {
                header: "Mode".into(),
                question: "Which mode?".into(),
                options: vec![],
                multi_select: false,
            }],
        };
        let a = ApprovalCard {
            ask_id: "approval".parse().unwrap(),
            tool_call_id: None,
            user_id: String::new(),
            tool: "Read".into(),
            accesses: vec![],
            params_preview: String::new(),
            description: None,
        };
        let snapshot = |pending_questions, pending_approvals| Frame::SubscribeState {
            session_id: session.clone(),
            as_of_ordinal: None,
            turn: TurnSnapshot {
                active: true,
                started_at: None,
            },
            work_steps: vec![],
            tasks: vec![],
            pending_questions,
            pending_approvals,
        };
        let live = |session_id| Frame::QuestionAsked {
            session_id,
            ask_id: q.ask_id.clone(),
            tool_call_id: q.tool_call_id.clone(),
            user_id: String::new(),
            questions: q.questions.clone(),
        };
        assert!(map_frame(live(sid("other")), &session, &asks).is_none());
        assert!(asks.head().is_none());
        map_frame(snapshot(vec![q.clone()], vec![a.clone()]), &session, &asks);
        map_frame(live(session.clone()), &session, &asks);
        state.refresh_asks();
        assert_eq!(state.question.as_ref().unwrap().id, q.ask_id);
        assert!(asks.answer_question(&q.ask_id, QuestionReply::Cancelled));
        assert!(!asks.answer_question(&q.ask_id, QuestionReply::Cancelled));
        assert_eq!(
            answers.try_recv().unwrap(),
            Frame::AnswerQuestion {
                ask_id: q.ask_id.clone(),
                reply: QuestionReply::Cancelled
            }
        );
        // The gateway re-sends its authoritative set if an answer is rejected.
        map_frame(snapshot(vec![q.clone()], vec![a.clone()]), &session, &asks);
        state.refresh_asks();
        assert!(!state.question.as_ref().unwrap().submitting);
        map_frame(
            Frame::AskResolved {
                ask_id: q.ask_id.clone(),
                decision: None,
                outcome: AskOutcomeKind::Abandoned,
            },
            &session,
            &asks,
        );
        assert!(!asks.answer_question(&q.ask_id, QuestionReply::Cancelled));
        state.refresh_asks();
        assert!(state.question.is_none());
        assert_eq!(state.pending_approval.as_ref().unwrap().ask_id, a.ask_id);
        map_frame(snapshot(vec![], vec![]), &session, &asks);
        state.refresh_asks();
        assert!(state.pending_approval.is_none());
        assert!(!asks.answer_approval(&a.ask_id, baybo_tools::ApprovalDecision::Approve));
        assert!(answers.try_recv().is_err());
    }

    #[test]
    fn map_frame_forwards_delta_for_pinned_session() {
        let (asks, _answers) = PendingAsks::new();
        let target = sid("alpha");
        let frame = Frame::AnswerDelta {
            session_id: target.clone(),
            user_id: String::new(),
            text: "tick".into(),
        };
        match map_frame(frame, &target, &asks) {
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
        let (asks, _answers) = PendingAsks::new();
        let target = sid("beta");
        let frame = Frame::AnswerDelta {
            session_id: sid("alpha"),
            user_id: String::new(),
            text: "stale".into(),
        };
        assert!(map_frame(frame, &target, &asks).is_none());
    }

    #[test]
    fn map_frame_drops_history_snapshot_silently() {
        // Subscribe always triggers a HistorySnapshot from the gateway
        // (including the `/new`-driven re-subscribe). Input history is
        // global to the TUI process, so the resend carries nothing the
        // pump cares about — it must be dropped, not warned about.
        let (asks, _answers) = PendingAsks::new();
        let target = sid("alpha");
        let frame = Frame::HistorySnapshot {
            session_id: target.clone(),
            entries: vec!["echo".into()],
        };
        assert!(map_frame(frame, &target, &asks).is_none());
    }

    #[test]
    fn map_frame_forwards_tool_lifecycle_for_pinned_session() {
        let (asks, _answers) = PendingAsks::new();
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
            &asks,
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
                approval: None,
                ask: None,
            },
            &target,
            &asks,
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
        let (asks, _answers) = PendingAsks::new();
        let target = sid("alpha");
        let frame = Frame::Reasoning {
            session_id: target.clone(),
            user_id: String::new(),
            text: "a thought".into(),
        };
        assert!(map_frame(frame, &target, &asks).is_none());
    }

    #[test]
    fn map_frame_drops_user_echo_for_pinned_session() {
        // Subscribed-kind channels echo inbound to every subscriber —
        // including the sender. The TUI already rendered its own
        // prompt locally, so the echo must not duplicate the row.
        let (asks, _answers) = PendingAsks::new();
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
        assert!(map_frame(echo, &target, &asks).is_none());
    }
}
