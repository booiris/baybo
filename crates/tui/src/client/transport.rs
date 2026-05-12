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

use aura_channels::wire::{Frame, Message as WireMessage};
use aura_channels::{ChannelError, IncomingMessage, NoticeLevel, Result};
use aura_model::{ChannelType, ContentBlock, SessionId};
use aura_tools::{ApprovalQueue, ApprovalRequest};
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
    /// Pinned session this TUI registered against. Used as the frame
    /// `session_id` on [`WsTransport::append_history`] so the gateway
    /// can attribute writes and enforce per-session auth invariants.
    session_id: SessionId,
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
            session_id,
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
            role: aura_channels::MessageRole::User,
            // Inbound to gateway — server picks up the ordinal on
            // persistence; outbound frames never need it set here.
            ordinal: None,
        };

        self.client
            .send(wire_msg)
            .await
            .map_err(|e| ChannelError::Send(format!("tui ws send: {e}")))
    }

    /// Open a long-lived subscription to gateway events for
    /// `session_id`. The returned stream ends when the peer closes.
    pub async fn subscribe(&self, session_id: &SessionId) -> Result<TransportEventStream> {
        let (tx, rx) = mpsc::channel::<Result<TransportEvent>>(EVENT_CHAN_CAPACITY);
        let client: Arc<WsClient> = Arc::clone(&self.client);
        let queue = self.approval_queue.clone();
        let target = session_id.clone();
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
                if let Some(event) = map_frame(frame, &target, &queue)
                    && tx.send(Ok(event)).await.is_err()
                {
                    return;
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)) as TransportEventStream)
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
            session_id: self.session_id.clone(),
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
        Frame::Delta {
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
            Some(TransportEvent::Response(vec![ContentBlock::Text(
                msg.content,
            )]))
        }
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
        Frame::ChatSessionListChanged { .. } => {
            // Web-chat sidebar pulse — TUI tracks a single session of
            // its own and has no list view, so it ignores this rather
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
        Frame::Register { .. }
        | Frame::RegisterAck { .. }
        | Frame::Subscribe { .. }
        | Frame::Unsubscribe { .. }
        | Frame::Reset { .. }
        | Frame::ResolveApproval { .. }
        | Frame::HistoryAppend { .. }
        | Frame::HistorySnapshot { .. }
        | Frame::StartBot { .. }
        | Frame::StopBot { .. }
        | Frame::BotStatus { .. }
        | Frame::SlashManifest { .. } => {
            // HistorySnapshot is drained during `connect_tui`; any
            // stray instance post-handshake is a protocol violation.
            // StartBot / StopBot / BotStatus are sidecar
            // control-plane frames — the TUI never participates in
            // that flow. SlashManifest is also sidecar-only: the TUI
            // owns its slash commands client-side via TuiSlashHandler.
            warn!("unexpected frame from gateway; dropping");
            None
        }
    }
}
