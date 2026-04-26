//! Per-connection gateway state for a live `/v1/channel-ws` sidecar.
//!
//! One [`Sidecar`] per connected WS client. The gateway owns:
//!
//! * An outbound frame mpsc. Every producer — the registered
//!   [`aura_channels::Channel`] (agent output), the approval-gate waker
//!   (`ApprovalRequested`), and the inbound loop's `resolve_approval`
//!   path (`ApprovalResolved`) — pushes a [`Frame`] here; one pump task
//!   drains the receiver and serializes each frame onto the WS sink.
//! * The approval queue shared with the [`ChannelApprovalGate`]; the
//!   inbound loop resolves entries by `call_id` when the client echoes
//!   a [`Frame::ResolveApproval`].
//!
//! Collapsing the old `ChannelAdapter` trait into a concrete type +
//! mpsc fan-in keeps `aura-channels` free of any wire-format knowledge.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::wire::{self, Frame, Message as WireMessage};
use aura_channels::{AgentOutput, Channel, ChannelError, NoticeLevel};
use aura_model::{ChannelType, ContentBlock};
use aura_tools::{ApprovalDecision, ApprovalGate, ApprovalQueue, ChannelApprovalGate};
use axum::extract::ws::{Message as AxumWsMessage, WebSocket};
use futures::SinkExt;
use futures::stream::SplitSink;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub(crate) type WsSink = SplitSink<WebSocket, AxumWsMessage>;

/// Matches the old HTTP adapter so operator muscle memory around
/// approval timing carries over.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Outbound mpsc buffer. Large enough to absorb a short burst of
/// deltas without back-pressuring the agent loop; small enough that a
/// dead WS sink drops frames rather than piling up unbounded.
const OUTBOUND_BUFFER: usize = 64;

/// Live state for one connected WS sidecar. Returned from
/// [`Sidecar::build`] alongside the spawned pump handle so the route
/// task can await / abort the pump on teardown.
pub(crate) struct Sidecar {
    pub channel: Arc<Channel>,
    approval_queue: ApprovalQueue,
    frame_tx: mpsc::Sender<Frame>,
    pump: JoinHandle<()>,
}

impl Sidecar {
    /// Build the sidecar and spawn the outbound pump. The pump owns
    /// the WS sink and exits cleanly once every `frame_tx` clone has
    /// dropped — the caller achieves this by unregistering the channel
    /// (clears the approval gate map) and dropping this struct.
    ///
    /// `session_id` picks the flavor of the resulting `Channel`:
    /// * `None` — sidecar that serves every session of `channel_type`
    ///   (Telegram sidecar, etc.).
    /// * `Some(sid)` — session-scoped client pinned to one session
    ///   (the built-in TUI).
    pub(crate) fn build(
        channel_type: ChannelType,
        session_id: Option<String>,
        sink: WsSink,
    ) -> Self {
        let (frame_tx, mut frame_rx) = mpsc::channel::<Frame>(OUTBOUND_BUFFER);
        let (output_tx, mut output_rx) = mpsc::channel::<AgentOutput>(OUTBOUND_BUFFER);
        let approval_queue = ApprovalQueue::new();

        // Translator: AgentOutput → Frame. Exits when every Arc<Channel>
        // drops (which closes `output_tx`).
        let translator_tx = frame_tx.clone();
        let translator_ct = channel_type.clone();
        tokio::spawn(async move {
            while let Some(output) = output_rx.recv().await {
                let frame = agent_output_to_frame(output, &translator_ct);
                if translator_tx.send(frame).await.is_err() {
                    break;
                }
            }
        });

        let gate = build_approval_gate(approval_queue.clone(), frame_tx.clone());
        let approval_gate: Arc<dyn ApprovalGate> = Arc::new(gate);

        let channel = Arc::new(match session_id {
            Some(sid) => {
                Channel::new_session_scoped(channel_type, sid, output_tx, Some(approval_gate))
            }
            None => Channel::new(channel_type, output_tx, Some(approval_gate)),
        });

        let pump = tokio::spawn(async move {
            let mut sink = sink;
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
        });

        Self {
            channel,
            approval_queue,
            frame_tx,
            pump,
        }
    }

    /// Push a frame directly to the outbound pump. Used for RegisterAck
    /// echoes during the handshake.
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

    /// Resolve a pending approval and echo `ApprovalResolved`. Called
    /// from the inbound loop when the client sends `ResolveApproval`.
    pub(crate) async fn resolve_approval(&self, call_id: &str, decision: ApprovalDecision) -> bool {
        let resolved = self.approval_queue.resolve_by_call_id(call_id, decision);
        if resolved {
            let frame = Frame::ApprovalResolved {
                call_id: call_id.to_owned(),
                decision,
            };
            if self.frame_tx.send(frame).await.is_err() {
                tracing::debug!(call_id, "ApprovalResolved send failed; pump closed");
            }
        }
        resolved
    }

    /// Split the pump off so the caller can await it after dropping
    /// the sidecar struct (which releases the internal `frame_tx`
    /// clone, letting the pump exit).
    pub(crate) fn into_pump(self) -> JoinHandle<()> {
        // Drop frame_tx explicitly so the outbound buffer is no longer
        // held by us — translator and approval-gate clones still exist
        // but will drop naturally as `channel` ref-count hits zero and
        // the gate map evicts the gate.
        drop(self.frame_tx);
        drop(self.channel);
        drop(self.approval_queue);
        self.pump
    }
}

fn build_approval_gate(queue: ApprovalQueue, frame_tx: mpsc::Sender<Frame>) -> ChannelApprovalGate {
    let waker_queue = queue.clone();
    let waker_tx = frame_tx;
    ChannelApprovalGate::new(
        queue,
        Arc::new(move || {
            // Snapshot the just-pushed entry. The waker fires
            // synchronously right after the enqueue so this is
            // guaranteed present unless a concurrent resolver drained
            // it already.
            let Some(entry) = waker_queue.list().into_iter().next_back() else {
                return;
            };
            let tx = waker_tx.clone();
            tokio::spawn(async move {
                let frame = Frame::ApprovalRequested {
                    call_id: entry.call_id,
                    session_id: entry.session_id,
                    user_id: entry.user_id,
                    tool: entry.tool,
                    accesses: entry.accesses,
                    params_preview: entry.params_preview,
                    description: entry.description,
                };
                let _ = tx.send(frame).await;
            });
        }),
        APPROVAL_TIMEOUT,
    )
}

fn agent_output_to_frame(output: AgentOutput, channel_type: &ChannelType) -> Frame {
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
            let content = flatten_content(&response.content);
            Frame::Message(WireMessage {
                content,
                session_id: response.session_id,
                // Populate with the addressee so sidecars can route by
                // user (Telegram: `user_id → chat_id`) without having to
                // maintain a `session_id → user_id` reverse map on their
                // side. Empty string on non-user-addressed emissions.
                user_id: response.user_id,
                channel_type: channel_type.clone(),
                // Outbound messages don't need `bot_id` — the sidecar
                // recovers it from its own `user_id → bot_id` map.
                bot_id: String::new(),
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

fn flatten_content(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}
