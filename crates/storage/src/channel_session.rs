//! Storage for channel-native-id → aura-session-id mappings.
//!
//! When a channel sidecar (e.g. Telegram) forwards a user's message, it
//! identifies the user by the platform-native id (Telegram user id,
//! prefixed as `tg_<id>`). The gateway keys aura's session UUID on the
//! pair `(channel_type, user_id)` so the same user always lands on the
//! same session — across sidecar restarts, across gateway restarts,
//! and without the sidecar needing to generate UUIDs itself.
//!
//! See `crates/gateway/src/channel/route.rs` for how this store is
//! consulted when an inbound `Frame::Message` arrives with an empty
//! `session_id`.

use async_trait::async_trait;
use aura_model::{ChannelType, SessionId};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

#[async_trait]
pub trait ChannelSessionStore: Send + Sync {
    /// Look up the aura `session_id` for `(channel_type, user_id)`.
    /// Returns `Ok(None)` when no mapping exists.
    async fn get(&self, channel_type: &ChannelType, user_id: &str) -> Result<Option<SessionId>>;

    /// Persist `session_id` under `(channel_type, user_id)`. On
    /// conflict with a live row carrying a *different* `session_id`,
    /// the existing mapping wins — callers should call `get` first and
    /// only `put` on a cache miss.
    async fn put(
        &self,
        channel_type: &ChannelType,
        user_id: &str,
        session_id: &SessionId,
    ) -> Result<()>;

    /// Hard-delete the mapping. Later `put`s for the same pair create
    /// a fresh row.
    async fn delete(&self, channel_type: &ChannelType, user_id: &str) -> Result<()>;
}
