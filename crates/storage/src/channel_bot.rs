//! Per-tenant credential registry for channel sidecars.
//!
//! Channels that multiplex many tenants over one sidecar (Telegram
//! bots, future Discord guilds, …) look up the active tenant set
//! here. The token itself lives in [`aura_security::SecretVault`] —
//! this table just records which bot ids exist and when they were
//! created. Callers join the two at runtime via a well-known secret
//! name pattern (`channel.<channel_type>.bot.<bot_id>.token`).

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
///
/// `revision` is a monotonic per-row counter the gateway reconciler
/// reads to detect credential / metadata rotations on a still-live
/// row. Every `put` bumps it; the bot reconciler restarts the running
/// sidecar bot when the persisted revision overtakes what it last
/// pushed, so a re-registration with rotated tokens or new auxiliary
/// secrets actually reaches the sidecar instead of being treated as a
/// no-op "already running" duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelBotRow {
    pub channel_type: ChannelType,
    pub bot_id: String,
    pub created_at: i64,
    pub revision: i64,
    pub metadata: HashMap<String, String>,
}

#[async_trait]
pub trait ChannelBotStore: Send + Sync {
    /// List every registered bot for the given channel type, newest first.
    async fn list_live(&self, channel_type: &ChannelType) -> Result<Vec<ChannelBotRow>>;

    /// Return the single bot's metadata if it's registered.
    async fn get(&self, channel_type: &ChannelType, bot_id: &str) -> Result<Option<ChannelBotRow>>;

    /// Register a bot. Idempotent on live-row conflict: the existing
    /// row's `created_at` wins but `metadata` is overwritten with the
    /// supplied value — this is the upsert path the CLI / WebUI uses
    /// to amend a registration's auxiliary configuration without
    /// losing the row's identity. `metadata` is a free-form
    /// `HashMap<String, String>`; empty maps are stored as `{}` and
    /// round-trip equivalently.
    async fn put(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        metadata: HashMap<String, String>,
    ) -> Result<()>;

    /// Hard-delete the bot. Later `put`s create a fresh row.
    async fn delete(&self, channel_type: &ChannelType, bot_id: &str) -> Result<()>;
}
