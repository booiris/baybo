#[cfg(feature = "telegram")]
use std::collections::HashMap;
#[cfg(feature = "telegram")]
use std::sync::Arc;
#[cfg(feature = "telegram")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "telegram")]
use async_trait::async_trait;
#[cfg(feature = "telegram")]
use aura_core::{
    ChannelMetadata, ChannelType, ContentBlock, IncomingMessage, Message, MessageMetadata,
    OutgoingMessage, User,
};
#[cfg(feature = "telegram")]
use teloxide::prelude::*;
#[cfg(feature = "telegram")]
use teloxide::types::InputFile;
#[cfg(feature = "telegram")]
use tokio::sync::mpsc;

#[cfg(feature = "telegram")]
use crate::ChannelAdapter;

/// Telegram channel adapter backed by `teloxide` with long polling.
#[cfg(feature = "telegram")]
pub struct TelegramChannel {
    bot_token: String,
    shutdown: Arc<AtomicBool>,
    /// Maps session_id (`tg_{chat_id}`) → `ChatId` for outbound routing.
    chat_ids: Arc<tokio::sync::RwLock<HashMap<String, ChatId>>>,
}

#[cfg(feature = "telegram")]
impl TelegramChannel {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            shutdown: Arc::new(AtomicBool::new(false)),
            chat_ids: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Convert a teloxide `Message` into an `IncomingMessage`.
    fn convert_message(msg: &teloxide::types::Message) -> Option<IncomingMessage> {
        let chat_id = msg.chat.id;
        let from = msg.from.as_ref()?;

        let session_id = format!("tg_{}", chat_id.0);
        let message_id = format!("tg_{}", msg.id.0);

        let sender = User {
            id: format!("tg_{}", from.id.0),
            name: Some(from.first_name.clone()),
            channel: ChannelType::Telegram,
        };

        let content = if let Some(text) = &msg.text() {
            vec![ContentBlock::Text(text.to_string())]
        } else {
            // For non-text messages (photo, audio, document) we create a
            // placeholder text block. Full media download is left for a future
            // iteration since it requires network calls to Telegram's file API.
            vec![ContentBlock::Text("[unsupported media]".to_string())]
        };

        let metadata = MessageMetadata {
            channel_specific: Some(ChannelMetadata {
                platform_message_id: Some(message_id.clone()),
                extra: HashMap::new(),
            }),
            ..Default::default()
        };

        Some(IncomingMessage {
            message: Message {
                id: message_id,
                session_id,
                channel: ChannelType::Telegram,
                sender,
                content,
                timestamp: chrono::Utc::now(),
                reply_to: msg.reply_to_message().map(|r| format!("tg_{}", r.id.0)),
                metadata,
            },
        })
    }
}

#[cfg(feature = "telegram")]
#[async_trait]
impl ChannelAdapter for TelegramChannel {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Telegram
    }

    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> aura_core::Result<()> {
        let bot = Bot::new(&self.bot_token);
        let shutdown = Arc::clone(&self.shutdown);
        let chat_ids = Arc::clone(&self.chat_ids);

        // Reset shutdown flag when (re-)starting.
        shutdown.store(false, Ordering::SeqCst);

        tokio::spawn(async move {
            let handler = Update::filter_message().endpoint(
                move |msg: teloxide::types::Message, _bot: Bot| {
                    let sender = sender.clone();
                    let chat_ids = chat_ids.clone();
                    async move {
                        let chat_id = msg.chat.id;
                        let session_id = format!("tg_{}", chat_id.0);

                        // Remember chat_id for outbound routing.
                        chat_ids.write().await.insert(session_id, chat_id);

                        if let Some(incoming) = TelegramChannel::convert_message(&msg)
                            && let Err(e) = sender.send(incoming).await
                        {
                            tracing::error!("failed to forward telegram message: {}", e);
                        }

                        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                    }
                },
            );

            let mut dispatcher = Dispatcher::builder(bot, handler)
                .enable_ctrlc_handler()
                .build();

            dispatcher.dispatch().await;
        });

        tracing::info!("telegram long-polling listener spawned");
        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> aura_core::Result<()> {
        let chat_id = {
            let map = self.chat_ids.read().await;
            map.get(&response.session_id).copied()
        };

        let chat_id = chat_id.ok_or_else(|| {
            aura_core::AuraError::NotFound(format!(
                "no chat_id mapped for session {}",
                response.session_id
            ))
        })?;

        let bot = Bot::new(&self.bot_token);

        for block in &response.content {
            match block {
                ContentBlock::Text(text) => {
                    bot.send_message(chat_id, text).await.map_err(|e| {
                        aura_core::AuraError::Config(format!("telegram send_message failed: {e}"))
                    })?;
                }
                ContentBlock::Image { blob, mime_type: _ } => {
                    // Use blob_id as the file-id / URL. Full blob resolution
                    // would require integration with a storage layer.
                    bot.send_photo(
                        chat_id,
                        InputFile::url(blob.blob_id.parse().map_err(|e: url::ParseError| {
                            aura_core::AuraError::Config(format!("invalid image url: {e}"))
                        })?),
                    )
                    .await
                    .map_err(|e| {
                        aura_core::AuraError::Config(format!("telegram send_photo failed: {e}"))
                    })?;
                }
                ContentBlock::Audio { blob, mime_type: _ } => {
                    bot.send_audio(
                        chat_id,
                        InputFile::url(blob.blob_id.parse().map_err(|e: url::ParseError| {
                            aura_core::AuraError::Config(format!("invalid audio url: {e}"))
                        })?),
                    )
                    .await
                    .map_err(|e| {
                        aura_core::AuraError::Config(format!("telegram send_audio failed: {e}"))
                    })?;
                }
                ContentBlock::File {
                    blob,
                    filename,
                    mime_type: _,
                } => {
                    let input =
                        InputFile::url(blob.blob_id.parse().map_err(|e: url::ParseError| {
                            aura_core::AuraError::Config(format!("invalid file url: {e}"))
                        })?)
                        .file_name(filename.clone());

                    bot.send_document(chat_id, input).await.map_err(|e| {
                        aura_core::AuraError::Config(format!("telegram send_document failed: {e}"))
                    })?;
                }
            }
        }

        Ok(())
    }

    async fn stop(&self) -> aura_core::Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        tracing::info!("telegram channel stop requested");
        Ok(())
    }
}

#[cfg(all(test, feature = "telegram"))]
mod tests {
    use super::*;

    #[test]
    fn channel_type_returns_telegram() {
        let ch = TelegramChannel::new("test-token".into());
        assert_eq!(ch.channel_type(), ChannelType::Telegram);
    }

    #[test]
    fn construction_works() {
        let ch = TelegramChannel::new("my-bot-token".into());
        assert_eq!(ch.bot_token, "my-bot-token");
        assert!(!ch.shutdown.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let ch = TelegramChannel::new("tok".into());
        ch.stop().await.ok();
        ch.stop().await.ok();
        assert!(ch.shutdown.load(Ordering::SeqCst));
    }
}
