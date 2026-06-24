//! Per-tenant credential registry for channel sidecars.
//!
//! Channels that multiplex many tenants over one sidecar (Telegram
//! bots, future Discord guilds, …) look up the active tenant set
//! here. The token itself lives in `baybo_security::SecretVault` —
//! this table just records which bot ids exist and when they were
//! created. Callers join the two at runtime via a well-known secret
//! name pattern (`channel.<channel_type>.bot.<bot_id>.token`).

use async_trait::async_trait;
use baybo_model::ChannelType;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Live metadata for one registered tenant. The token is NOT
/// included — callers fetch it from the vault separately so we never
/// hand plaintext tokens around the in-memory graph longer than
/// necessary.
#[derive(Debug, Clone)]
pub struct ChannelBotRow {
    pub channel_type: ChannelType,
    pub bot_id: String,
    pub created_at: i64,
}

#[async_trait]
pub trait ChannelBotStore: Send + Sync {
    /// List every registered bot for the given channel type, newest first.
    async fn list_live(&self, channel_type: &ChannelType) -> Result<Vec<ChannelBotRow>>;

    /// Return the single bot's metadata if it's registered.
    async fn get(&self, channel_type: &ChannelType, bot_id: &str) -> Result<Option<ChannelBotRow>>;

    /// Register a bot. Idempotent: on a live-row conflict the existing
    /// row wins (no-op).
    async fn put(&self, channel_type: &ChannelType, bot_id: &str) -> Result<()>;

    /// Hard-delete the bot. Later `put`s create a fresh row.
    async fn delete(&self, channel_type: &ChannelType, bot_id: &str) -> Result<()>;
}
