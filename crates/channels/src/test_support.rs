//! Spy `ChannelAdapter` implementations for downstream tests.
//!
//! `RecordingChannel` captures every `OutgoingMessage`, stream delta,
//! and notice the router pushes; tests then assert against the captured
//! sequence. Construction is cheap and thread-safe.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use aura_model::ChannelType;
use tokio::sync::mpsc;

use crate::types::{IncomingMessage, NoticeLevel, OutgoingMessage};
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
        self.responses.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn deltas(&self) -> Vec<DeltaRecord> {
        self.deltas.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Concatenated delta text in arrival order. Useful to assert the
    /// full streamed response (placeholder-form) without splicing.
    pub fn delta_text(&self) -> String {
        self.deltas
            .lock()
            .map(|v| v.iter().map(|d| d.text.as_str()).collect::<String>())
            .unwrap_or_default()
    }

    pub fn notices(&self) -> Vec<NoticeRecord> {
        self.notices.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl ChannelAdapter for RecordingChannel {
    fn channel_type(&self) -> ChannelType {
        self.channel_type
    }

    async fn start(&self, _sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> Result<()> {
        if let Ok(mut v) = self.responses.lock() {
            v.push(response);
        }
        Ok(())
    }

    async fn send_stream_delta(&self, session_id: &str, delta: &str) -> Result<()> {
        if let Ok(mut v) = self.deltas.lock() {
            v.push(DeltaRecord {
                session_id: session_id.to_owned(),
                text: delta.to_owned(),
            });
        }
        Ok(())
    }

    async fn send_notice(&self, session_id: &str, level: NoticeLevel, text: &str) -> Result<()> {
        if let Ok(mut v) = self.notices.lock() {
            v.push(NoticeRecord {
                session_id: session_id.to_owned(),
                level,
                text: text.to_owned(),
            });
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
            channel: ChannelType::Tui,
            content: vec![ContentBlock::Text(text.into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        }
    }

    #[tokio::test]
    async fn captures_responses_deltas_notices() {
        let ch = RecordingChannel::new(ChannelType::Tui);
        ch.send_response(outgoing("s1", "hello")).await.unwrap();
        ch.send_stream_delta("s1", "he").await.unwrap();
        ch.send_stream_delta("s1", "llo").await.unwrap();
        ch.send_notice("s1", NoticeLevel::Warn, "careful")
            .await
            .unwrap();

        assert_eq!(ch.responses().len(), 1);
        assert_eq!(ch.delta_text(), "hello");
        assert_eq!(ch.notices().len(), 1);
        assert_eq!(ch.notices()[0].level, NoticeLevel::Warn);
    }
}
