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

use std::path::PathBuf;
use std::sync::Arc;

use aura_channels::wire::{Frame, Message as WireMessage};
use aura_channels::{ChannelError, IncomingMessage, NoticeLevel, Result};
use aura_model::{ChannelType, ContentBlock};
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
}

impl WsTransport {
    /// Dial the channel UDS with the TUI PSK and register as the
    /// built-in `"tui"` channel. Returns a ready transport.
    pub async fn connect(socket_path: PathBuf, psk: [u8; 32]) -> Result<Self> {
        let client = WsClient::connect_tui(&socket_path, &psk, ChannelType::from("tui"))
            .await
            .map_err(|e| ChannelError::Config(format!("tui ws connect: {e}")))?;
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
        };

        self.client
            .send(wire_msg)
            .await
            .map_err(|e| ChannelError::Send(format!("tui ws send: {e}")))
    }

    /// Open a long-lived subscription to gateway events for
    /// `session_id`. The returned stream ends when the peer closes.
    pub async fn subscribe(&self, session_id: &str) -> Result<TransportEventStream> {
        let (tx, rx) = mpsc::channel::<Result<TransportEvent>>(EVENT_CHAN_CAPACITY);
        let client: Arc<WsClient> = Arc::clone(&self.client);
        let queue = self.approval_queue.clone();
        let target = session_id.to_owned();
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
}

fn map_frame(frame: Frame, target_session: &str, queue: &ApprovalQueue) -> Option<TransportEvent> {
    match frame {
        Frame::Delta { session_id, text } => {
            if session_id != target_session {
                return None;
            }
            Some(TransportEvent::StreamDelta(text))
        }
        Frame::Message(msg) => {
            if msg.session_id != target_session {
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
        } => {
            if session_id != target_session {
                return None;
            }
            let level = match level.as_str() {
                "error" => NoticeLevel::Error,
                _ => NoticeLevel::Warn,
            };
            Some(TransportEvent::Notice { level, text })
        }
        Frame::ApprovalRequested {
            call_id,
            session_id,
            tool,
            accesses,
            params_preview,
        } => {
            if session_id != target_session {
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
                tool,
                accesses,
                params_preview,
            });
            Some(TransportEvent::ApprovalRequested)
        }
        Frame::ApprovalResolved { call_id, decision } => {
            let _ = queue.drop_call(&call_id);
            Some(TransportEvent::ApprovalResolved { call_id, decision })
        }
        Frame::Register { .. } | Frame::RegisterAck { .. } | Frame::ResolveApproval { .. } => {
            warn!("unexpected frame from gateway; dropping");
            None
        }
    }
}
