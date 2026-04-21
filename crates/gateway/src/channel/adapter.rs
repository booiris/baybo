//! `ChannelAdapter` implementation for a single connected WS sidecar.
//!
//! One instance per live `/v1/channel-ws` connection. The route owns
//! the inbound half (reading frames and pushing `IncomingMessage`s into
//! the router); this adapter owns the outbound half (forwarding
//! `AgentOutput` back through the WebSocket sink).
//!
//! Limitations carried forward from the v1 wire protocol:
//! * `AgentOutput::Delta` is coalesced into a single `Message` frame —
//!   sidecars needing per-token rendering must wait for a dedicated
//!   delta frame kind.
//! * `approval_gate()` returns `None`; interactive approval from
//!   sidecars is out of scope for v1 and arrives separately.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use aura_channels::sdk::wire::{self, Frame, Message as WireMessage};
use aura_channels::{
    AgentOutput, ChannelAdapter, ChannelError, IncomingMessage, NoticeLevel, Result,
};
use aura_model::{ChannelType, ContentBlock};
use aura_tools::ApprovalGate;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket};
use futures::SinkExt;
use futures::stream::SplitSink;
use parking_lot::Mutex;
use tokio::sync::mpsc;

pub(crate) type WsSink = SplitSink<WebSocket, AxumWsMessage>;

pub(crate) struct SidecarAdapter {
    channel_type: ChannelType,
    sink: Arc<tokio::sync::Mutex<WsSink>>,
    closed: AtomicBool,
    /// Session-id tags we inject into `Delta`/`Notice` frames the
    /// sidecar can't otherwise carry. Stored so `send` can attribute
    /// to the right session when a frame doesn't already have one.
    last_session: Mutex<Option<String>>,
}

impl SidecarAdapter {
    pub(crate) fn new(channel_type: ChannelType, sink: WsSink) -> Self {
        Self {
            channel_type,
            sink: Arc::new(tokio::sync::Mutex::new(sink)),
            closed: AtomicBool::new(false),
            last_session: Mutex::new(None),
        }
    }

    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub(crate) async fn send_frame(&self, frame: Frame) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ChannelError::Config("ws sidecar closed".into()));
        }
        let bytes =
            wire::encode(&frame).map_err(|e| ChannelError::Config(format!("encode frame: {e}")))?;
        let mut sink = self.sink.lock().await;
        sink.send(AxumWsMessage::Binary(bytes.into()))
            .await
            .map_err(|e| {
                self.closed.store(true, Ordering::SeqCst);
                ChannelError::Config(format!("ws sink: {e}"))
            })
    }
}

#[async_trait]
impl ChannelAdapter for SidecarAdapter {
    fn channel_type(&self) -> ChannelType {
        self.channel_type.clone()
    }

    async fn start(&self, _sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        // Inbound plumbing is owned by the WS route task because the
        // connection already exists by the time `register()` runs.
        tracing::debug!(
            channel_type = %self.channel_type,
            "sidecar adapter started (route task owns inbound loop)",
        );
        Ok(())
    }

    async fn send(&self, output: AgentOutput) -> Result<()> {
        let channel_type = self.channel_type.clone();
        let frame = match output {
            AgentOutput::Delta {
                session_id, text, ..
            } => {
                *self.last_session.lock() = Some(session_id.clone());
                Frame::Message(WireMessage {
                    content: text,
                    session_id,
                    user_id: String::new(),
                    channel_type,
                })
            }
            AgentOutput::Message(response) => {
                let content = flatten_content(&response.content);
                *self.last_session.lock() = Some(response.session_id.clone());
                Frame::Message(WireMessage {
                    content,
                    session_id: response.session_id,
                    user_id: String::new(),
                    channel_type,
                })
            }
            AgentOutput::Notice {
                session_id,
                level,
                text,
                ..
            } => {
                let tag = match level {
                    NoticeLevel::Warn => "warn",
                    NoticeLevel::Error => "error",
                };
                Frame::Message(WireMessage {
                    content: format!("[{tag}] {text}"),
                    session_id,
                    user_id: String::new(),
                    channel_type,
                })
            }
        };
        self.send_frame(frame).await
    }

    fn approval_gate(&self) -> Option<Arc<dyn ApprovalGate>> {
        None
    }

    async fn stop(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        let mut sink = self.sink.lock().await;
        let _ = sink.close().await;
        Ok(())
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
