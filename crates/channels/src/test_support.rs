//! Spy `ChannelAdapter` implementations for downstream tests.
//!
//! `RecordingChannel` captures every `OutgoingMessage`, stream delta,
//! and notice the router pushes; tests then assert against the captured
//! sequence. Construction is cheap and thread-safe.

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::ChannelType;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::types::{AgentOutput, IncomingMessage, NoticeLevel, OutgoingMessage};
use crate::{ChannelAdapter, Result};

/// Snapshot of one delta call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaRecord {
    pub session_id: String,
    pub text: String,
}

/// Snapshot of one notice call.
#[derive(Debug, Clone)]
pub struct NoticeRecord {
    pub session_id: String,
    pub level: NoticeLevel,
    pub text: String,
}

/// Records everything sent through the channel for later inspection.
/// Cloning is cheap — clones share the same backing buffers via `Arc`.
#[derive(Debug, Clone)]
pub struct RecordingChannel {
    channel_type: ChannelType,
    responses: Arc<Mutex<Vec<OutgoingMessage>>>,
    deltas: Arc<Mutex<Vec<DeltaRecord>>>,
    notices: Arc<Mutex<Vec<NoticeRecord>>>,
}

impl RecordingChannel {
    pub fn new(channel_type: ChannelType) -> Self {
        Self {
            channel_type,
            responses: Arc::new(Mutex::new(Vec::new())),
            deltas: Arc::new(Mutex::new(Vec::new())),
            notices: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn responses(&self) -> Vec<OutgoingMessage> {
        self.responses.lock().to_vec()
    }

    pub fn deltas(&self) -> Vec<DeltaRecord> {
        self.deltas.lock().to_vec()
    }

    /// Concatenated delta text in arrival order. Useful to assert the
    /// full streamed response (placeholder-form) without splicing.
    pub fn delta_text(&self) -> String {
        let guard = self.deltas.lock();
        guard.iter().map(|d| d.text.as_str()).collect::<String>()
    }

    pub fn notices(&self) -> Vec<NoticeRecord> {
        self.notices.lock().to_vec()
    }
}

#[async_trait]
impl ChannelAdapter for RecordingChannel {
    fn channel_type(&self) -> ChannelType {
        self.channel_type.clone()
    }

    async fn start(&self, _sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        Ok(())
    }

    async fn send(&self, output: AgentOutput) -> Result<()> {
        match output {
            AgentOutput::Message(response) => {
                self.responses.lock().push(response);
            }
            AgentOutput::Delta {
                session_id, text, ..
            } => {
                self.deltas.lock().push(DeltaRecord { session_id, text });
            }
            AgentOutput::Notice {
                session_id,
                level,
                text,
                ..
            } => {
                self.notices.lock().push(NoticeRecord {
                    session_id,
                    level,
                    text,
                });
            }
        }
        Ok(())
    }

    fn approval_gate(&self) -> Option<Arc<dyn aura_tools::ApprovalGate>> {
        None
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, ContentBlock, MessageMetadata};

    fn outgoing(session: &str, text: &str) -> OutgoingMessage {
        OutgoingMessage {
            session_id: session.into(),
            channel: ChannelType::tui(),
            content: vec![ContentBlock::Text(text.into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        }
    }

    fn delta(session: &str, text: &str) -> AgentOutput {
        AgentOutput::Delta {
            session_id: session.into(),
            channel: ChannelType::tui(),
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn captures_responses_deltas_notices() {
        let ch = RecordingChannel::new(ChannelType::tui());
        ch.send(AgentOutput::Message(outgoing("s1", "hello")))
            .await
            .unwrap();
        ch.send(delta("s1", "he")).await.unwrap();
        ch.send(delta("s1", "llo")).await.unwrap();
        ch.send(AgentOutput::Notice {
            session_id: "s1".into(),
            channel: ChannelType::tui(),
            level: NoticeLevel::Warn,
            text: "careful".into(),
        })
        .await
        .unwrap();

        assert_eq!(ch.responses().len(), 1);
        assert_eq!(ch.delta_text(), "hello");
        assert_eq!(ch.notices().len(), 1);
        assert_eq!(ch.notices()[0].level, NoticeLevel::Warn);
    }
}
