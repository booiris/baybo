use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{ContentBlock, MessageMetadata};
use aura_session::{ChannelType, User};
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

use crate::{IncomingMessage, Message, OutgoingMessage, Result};

/// CLI channel adapter that reads from stdin and writes to stdout.
///
/// When `start()` is called a background task reads lines from stdin,
/// wraps each line as an `IncomingMessage`, and pushes it into the
/// router's incoming channel.  `send_response()` formats the content
/// blocks and writes them to stdout.
pub struct CliAdapter {
    session_id: String,
    user: User,
    shutdown: Arc<Notify>,
}

impl Default for CliAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CliAdapter {
    pub fn new() -> Self {
        let user_id = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "cli-user".to_string());
        Self {
            session_id: format!("cli-{}", Uuid::new_v4()),
            user: User {
                id: user_id,
                name: None,
                channel: ChannelType::Cli,
            },
            shutdown: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl crate::ChannelAdapter for CliAdapter {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Cli
    }

    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let session_id = self.session_id.clone();
        let user = self.user.clone();
        let shutdown = Arc::clone(&self.shutdown);

        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin).lines();

            // Print prompt for the first line.
            {
                let mut stdout = tokio::io::stdout();
                let _ = stdout.write_all(b"\naura> ").await;
                let _ = stdout.flush().await;
            }

            loop {
                tokio::select! {
                    line = reader.next_line() => {
                        match line {
                            Ok(Some(text)) => {
                                let text = text.trim().to_string();
                                if text.is_empty() {
                                    let mut stdout = tokio::io::stdout();
                                    let _ = stdout.write_all(b"aura> ").await;
                                    let _ = stdout.flush().await;
                                    continue;
                                }
                                if text == "/quit" || text == "/exit" {
                                    break;
                                }
                                let msg = IncomingMessage {
                                    message: Message {
                                        id: Uuid::new_v4().to_string(),
                                        session_id: session_id.clone(),
                                        channel: ChannelType::Cli,
                                        sender: user.clone(),
                                        content: vec![ContentBlock::Text(text)],
                                        timestamp: Utc::now(),
                                        reply_to: None,
                                        metadata: MessageMetadata::default(),
                                    },
                                };
                                if sender.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break, // EOF
                            Err(_) => break,
                        }
                    }
                    _ = shutdown.notified() => {
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        for block in &response.content {
            match block {
                ContentBlock::Text(text) => {
                    let _ = stdout.write_all(b"\n").await;
                    let _ = stdout.write_all(text.as_bytes()).await;
                    let _ = stdout.write_all(b"\n").await;
                }
                ContentBlock::Image { mime_type, .. } => {
                    let _ = stdout
                        .write_all(format!("\n[Image: {mime_type}]\n").as_bytes())
                        .await;
                }
                ContentBlock::Audio { mime_type, .. } => {
                    let _ = stdout
                        .write_all(format!("\n[Audio: {mime_type}]\n").as_bytes())
                        .await;
                }
                ContentBlock::File {
                    filename,
                    mime_type,
                    ..
                } => {
                    let _ = stdout
                        .write_all(format!("\n[File: {filename} ({mime_type})]\n").as_bytes())
                        .await;
                }
            }
        }
        let _ = stdout.write_all(b"aura> ").await;
        let _ = stdout.flush().await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.notify_one();
        Ok(())
    }
}
