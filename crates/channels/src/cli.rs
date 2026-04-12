use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{ContentBlock, MessageMetadata};
use aura_session::{ChannelType, User};
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::{IncomingMessage, Message, OutgoingMessage, Result, SlashHandler, SlashOutcome};

/// CLI channel adapter that reads from stdin and writes to stdout.
///
/// When `start()` is called a background task reads lines from stdin,
/// wraps each line as an `IncomingMessage`, and pushes it into the
/// router's incoming channel.  `send_response()` formats the content
/// blocks and writes them to stdout.
///
/// A [`SlashHandler`] may be attached via [`CliAdapter::with_slash_handler`]
/// to intercept `/`-prefixed input. Reserved tokens `/quit`, `/exit`, and
/// `/clear` are handled by the adapter itself and never reach the handler.
pub struct CliAdapter {
    session_id: String,
    user: User,
    shutdown: Arc<Notify>,
    slash_handler: Option<Arc<dyn SlashHandler>>,
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
            slash_handler: None,
        }
    }

    /// Attach a slash-command handler. Returns `self` for builder-style chaining.
    pub fn with_slash_handler(mut self, handler: Arc<dyn SlashHandler>) -> Self {
        self.slash_handler = Some(handler);
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

async fn write_prompt() {
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(b"aura> ").await;
    let _ = stdout.flush().await;
}

async fn write_blocks(blocks: &[ContentBlock]) {
    let mut stdout = tokio::io::stdout();
    for block in blocks {
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
        let slash_handler = self.slash_handler.clone();

        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin).lines();

            // Initial prompt for the first line.
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
                                    write_prompt().await;
                                    continue;
                                }
                                // Adapter-local reserved tokens.
                                if text == "/quit" || text == "/exit" {
                                    break;
                                }
                                if text == "/clear" {
                                    let mut stdout = tokio::io::stdout();
                                    let _ = stdout.write_all(b"\x1b[2J\x1b[H").await;
                                    let _ = stdout.flush().await;
                                    write_prompt().await;
                                    continue;
                                }

                                // Slash dispatch.
                                if text.starts_with('/')
                                    && let Some(handler) = slash_handler.as_ref()
                                {
                                    match handler.handle(&text).await {
                                        SlashOutcome::Handled(blocks) => {
                                            write_blocks(&blocks).await;
                                            continue;
                                        }
                                        SlashOutcome::Exit => break,
                                        SlashOutcome::PassThrough => {}
                                    }
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
        write_blocks(&response.content).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.notify_one();
        Ok(())
    }
}
