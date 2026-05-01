//! Per-tenant credential registry for channel sidecars.
//!
//! Channels that multiplex many tenants over one sidecar (Telegram
//! bots, future Discord guilds, …) look up the active tenant set
//! here. The token itself lives in [`aura_security::SecretVault`] —
//! this table just records which bot ids exist, when they were
//! created, and whether they're soft-deleted. Callers join the two
//! at runtime via a well-known secret name pattern
//! (`channel.<channel_type>.bot.<bot_id>.token`).

use std::collections::HashMap;

use async_trait::async_trait;
use aura_model::ChannelType;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Live metadata for one registered tenant. The primary token is NOT
/// included — callers fetch it from the vault separately so we never
/// hand plaintext tokens around the in-memory graph longer than
/// necessary.
///
/// `metadata` is a free-form auxiliary key/value map plumbed through
/// to the sidecar on [`aura_channels::wire::Frame::StartBot.metadata`].
/// Multi-secret channels (Lark's `app_secret`/`encrypt_key`/…, Discord
/// intents bitmask, …) store any non-token configuration here. Empty
/// for single-secret channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelBotRow {
    pub channel_type: ChannelType,
    pub bot_id: String,
    pub created_at: i64,
    pub metadata: HashMap<String, String>,
}

#[async_trait]
pub trait ChannelBotStore: Send + Sync {
    /// List every live bot for the given channel type, newest first.
    async fn list_live(&self, channel_type: &ChannelType) -> Result<Vec<ChannelBotRow>>;

    /// Return the single bot's metadata if it's live.
    async fn get(&self, channel_type: &ChannelType, bot_id: &str) -> Result<Option<ChannelBotRow>>;

    /// Mark a bot as live. Idempotent: re-adding a tombstone row
    /// revives it (with a fresh `created_at`). On a live-row conflict
    /// the existing row's `created_at` wins but `metadata` is
    /// overwritten with the supplied value — this is the upsert path
    /// the CLI / WebUI uses to amend a registration's auxiliary
    /// configuration without losing the row's identity. `metadata` is
    /// a free-form `HashMap<String, String>`; empty maps are stored
    /// as `{}` and round-trip equivalently.
    async fn put(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        metadata: HashMap<String, String>,
    ) -> Result<()>;

    /// Soft-delete the bot. Later `put`s revive.
    async fn delete(&self, channel_type: &ChannelType, bot_id: &str) -> Result<()>;
}
