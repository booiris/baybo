use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use aura_core::{
    ChannelType, ContentBlock, IncomingMessage, Message, MessageMetadata, OutgoingMessage, User,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::ChannelAdapter;

/// CLI channel for local development and debugging.
///
/// Reads lines from stdin and converts them to `IncomingMessage`.
/// Prints text responses to stdout. Supports `/quit` to exit.
pub struct CliChannel {
    shutdown: Arc<AtomicBool>,
}

impl CliChannel {
    pub fn new() -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for CliChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelAdapter for CliChannel {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Cli
    }

    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> aura_core::Result<()> {
        let shutdown = Arc::clone(&self.shutdown);

        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin);
            let mut line = String::new();

            loop {
                if shutdown.load(Ordering::Relaxed) {
                    tracing::debug!("CLI channel shutdown flag set, exiting read loop");
                    break;
                }

                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        // EOF reached
                        tracing::info!("CLI channel received EOF, stopping");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        if trimmed == "/quit" {
                            tracing::info!("CLI channel received /quit command");
                            break;
                        }

                        let message_id = format!("cli_{}", uuid::Uuid::new_v4());
                        let incoming = IncomingMessage {
                            message: Message {
                                id: message_id,
                                session_id: "cli_default".to_owned(),
                                channel: ChannelType::Cli,
                                sender: User {
                                    id: "cli_user".to_owned(),
                                    name: Some("CLI User".to_owned()),
                                    channel: ChannelType::Cli,
                                },
                                content: vec![ContentBlock::Text(trimmed.to_owned())],
                                timestamp: chrono::Utc::now(),
                                reply_to: None,
                                metadata: MessageMetadata::default(),
                            },
                        };

                        if sender.send(incoming).await.is_err() {
                            tracing::warn!("CLI channel: receiver dropped, stopping");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("CLI channel: stdin read error: {e}");
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> aura_core::Result<()> {
        for block in &response.content {
            match block {
                ContentBlock::Text(text) => {
                    println!("{text}");
                }
                ContentBlock::Image { mime_type, .. } => {
                    println!("[image: {mime_type}]");
                }
                ContentBlock::Audio { mime_type, .. } => {
                    println!("[audio: {mime_type}]");
                }
                ContentBlock::File {
                    filename,
                    mime_type,
                    ..
                } => {
                    println!("[file: {filename} ({mime_type})]");
                }
            }
        }
        Ok(())
    }

    async fn stop(&self) -> aura_core::Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        tracing::info!("CLI channel stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_channel_default() {
        let channel = CliChannel::default();
        assert_eq!(channel.channel_type(), ChannelType::Cli);
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let channel = CliChannel::new();
        channel.stop().await.expect("first stop should succeed");
        channel.stop().await.expect("second stop should succeed");
        assert!(channel.shutdown.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn send_response_text() {
        let channel = CliChannel::new();
        let response = OutgoingMessage {
            session_id: "cli_default".to_owned(),
            channel: ChannelType::Cli,
            content: vec![ContentBlock::Text("hello".to_owned())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        };
        channel
            .send_response(response)
            .await
            .expect("send_response should succeed");
    }
}
