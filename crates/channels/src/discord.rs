use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use aura_model::{ContentBlock, MessageMetadata};
use aura_session::{ChannelType, User};
use serenity::all::{ChannelId, Context, CreateMessage, EventHandler, GatewayIntents, Ready};
use serenity::http::Http;
use tokio::sync::{RwLock, mpsc};

use crate::{ChannelAdapter, ChannelError, IncomingMessage, Message, OutgoingMessage, Result};

type ChannelMap = Arc<RwLock<HashMap<String, (ChannelId, Arc<Http>)>>>;

/// Discord channel adapter using serenity for Gateway WebSocket events.
///
/// Receives messages through Gateway events and sends replies through the
/// Discord message REST API. Feature-gated behind `discord`.
pub struct DiscordChannel {
    bot_token: String,
    shutdown: Arc<AtomicBool>,
    /// Maps session_id (`dc_{channel_id}`) to (ChannelId, Http) for sending responses.
    channel_map: ChannelMap,
}

impl DiscordChannel {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            shutdown: Arc::new(AtomicBool::new(false)),
            channel_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Build a session ID from a Discord channel ID.
    fn session_id(channel_id: ChannelId) -> String {
        format!("dc_{}", channel_id)
    }

    /// Build a message ID from a Discord message ID.
    fn message_id(msg_id: serenity::model::id::MessageId) -> String {
        format!("dc_{}", msg_id)
    }

    /// Build a user ID from a Discord author ID.
    fn user_id(author_id: serenity::model::id::UserId) -> String {
        format!("dc_{}", author_id)
    }
}

/// Internal serenity event handler that bridges Discord events to the channel adapter.
struct Handler {
    sender: mpsc::Sender<IncomingMessage>,
    channel_map: ChannelMap,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: serenity::model::channel::Message) {
        // Ignore messages from bots to prevent loops.
        if msg.author.bot {
            return;
        }

        let session_id = DiscordChannel::session_id(msg.channel_id);

        // Store the channel mapping for later response sending.
        {
            let mut map = self.channel_map.write().await;
            map.entry(session_id.clone())
                .or_insert_with(|| (msg.channel_id, Arc::clone(&ctx.http)));
        }

        let incoming = IncomingMessage {
            message: Message {
                id: DiscordChannel::message_id(msg.id),
                session_id,
                channel: ChannelType::Discord,
                sender: User {
                    id: DiscordChannel::user_id(msg.author.id),
                    name: Some(msg.author.name.clone()),
                    channel: ChannelType::Discord,
                },
                content: vec![ContentBlock::Text(msg.content.clone())],
                timestamp: chrono::Utc::now(),
                reply_to: msg
                    .referenced_message
                    .as_ref()
                    .map(|r| DiscordChannel::message_id(r.id)),
                metadata: MessageMetadata::default(),
            },
        };

        if self.sender.send(incoming).await.is_err() {
            tracing::warn!("Discord channel: receiver dropped, message not forwarded");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!("Discord bot connected as {}", ready.user.name);
    }
}

#[async_trait]
impl ChannelAdapter for DiscordChannel {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Discord
    }

    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let handler = Handler {
            sender,
            channel_map: Arc::clone(&self.channel_map),
        };

        let mut client = serenity::Client::builder(&self.bot_token, intents)
            .event_handler(handler)
            .await
            .map_err(|e| ChannelError::Config(format!("failed to create Discord client: {e}")))?;

        let shutdown = Arc::clone(&self.shutdown);
        let shard_manager = Arc::clone(&client.shard_manager);

        tokio::spawn(async move {
            if let Err(e) = client.start().await {
                tracing::error!("Discord client error: {e}");
            }
        });

        // Spawn a task to watch the shutdown flag and stop the shard manager.
        tokio::spawn(async move {
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    shard_manager.shutdown_all().await;
                    tracing::info!("Discord shard manager shut down");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        });

        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> Result<()> {
        let map = self.channel_map.read().await;
        let (channel_id, http) = map.get(&response.session_id).ok_or_else(|| {
            ChannelError::Send(format!(
                "no Discord channel mapping for session {}",
                response.session_id
            ))
        })?;

        let mut text_parts = Vec::new();
        for block in &response.content {
            match block {
                ContentBlock::Text(text) => {
                    text_parts.push(text.clone());
                }
                ContentBlock::Image { mime_type, .. } => {
                    text_parts.push(format!("[image: {mime_type}]"));
                }
                ContentBlock::Audio { mime_type, .. } => {
                    text_parts.push(format!("[audio: {mime_type}]"));
                }
                ContentBlock::File {
                    filename,
                    mime_type,
                    ..
                } => {
                    text_parts.push(format!("[file: {filename} ({mime_type})]"));
                }
            }
        }

        let full_text = text_parts.join("\n");
        if full_text.is_empty() {
            return Ok(());
        }

        // Discord has a 2000-character message limit. Split if necessary.
        let chunks = split_message(&full_text, 2000);
        for chunk in chunks {
            let builder = CreateMessage::new().content(chunk);
            channel_id
                .send_message(&**http, builder)
                .await
                .map_err(|e| {
                    ChannelError::Internal(anyhow::anyhow!(
                        "failed to send Discord message to {}: {e}",
                        channel_id
                    ))
                })?;
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        tracing::info!("Discord channel stopped");
        Ok(())
    }
}

/// Split a message into chunks that fit within Discord's character limit.
fn split_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining);
            break;
        }
        // Try to split at a newline boundary within the limit.
        let split_at = remaining[..max_len]
            .rfind('\n')
            .map(|pos| pos + 1)
            .unwrap_or(max_len);
        chunks.push(&remaining[..split_at]);
        remaining = &remaining[split_at..];
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_returns_discord() {
        let channel = DiscordChannel::new("test-token".to_owned());
        assert_eq!(channel.channel_type(), ChannelType::Discord);
    }

    #[test]
    fn construction_works() {
        let channel = DiscordChannel::new("bot-token-123".to_owned());
        assert_eq!(channel.bot_token, "bot-token-123");
        assert!(!channel.shutdown.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let channel = DiscordChannel::new("test-token".to_owned());
        channel.stop().await.expect("first stop should succeed");
        channel.stop().await.expect("second stop should succeed");
        assert!(channel.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn split_message_short() {
        let chunks = split_message("hello", 2000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_message_long() {
        let text = "a".repeat(2500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 2000);
        assert_eq!(chunks[1].len(), 500);
    }

    #[test]
    fn split_message_at_newline() {
        let mut text = "a".repeat(1990);
        text.push('\n');
        text.push_str(&"b".repeat(100));
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('\n'));
    }

    #[test]
    fn id_mappings() {
        assert_eq!(
            DiscordChannel::session_id(ChannelId::new(12345)),
            "dc_12345"
        );
    }
}
