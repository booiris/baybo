//! Resolves `(channel_type, user_id)` → aura `session_id` for sidecars
//! that don't carry their own session ids on the wire.
//!
//! The TUI is session-scoped — it picks its own UUID and mints it into
//! the Register frame. Third-party sidecars (the Telegram channel at
//! `channel-src/telegram/` is the canonical example) have no natural
//! way to generate stable UUIDs per end-user, so they send
//! `Frame::Message { session_id: "", user_id: "<platform_user>" }`
//! and let the gateway resolve / allocate on their behalf. The mapping
//! is persisted in the `channel_sessions` libsql table so:
//!
//! * Same Telegram user → same aura session across sidecar restarts.
//! * Same Telegram user → same aura session across gateway restarts
//!   (libsql is the source of truth, not in-memory state).
//! * Different users on the same bot always land on distinct sessions.

use std::sync::Arc;

use aura_agent::SessionManager;
use aura_model::{ChannelType, User};
use aura_storage::ChannelSessionStore;

/// Errors surfaced from [`ChannelSessionResolver::resolve_or_create`].
/// Kept narrow on purpose — the caller just needs to decide whether to
/// abort the inbound frame or let the message through.
#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("session manager: {0}")]
    SessionManager(String),

    #[error("channel session store: {0}")]
    Store(String),
}

pub struct ChannelSessionResolver {
    sessions: Arc<SessionManager>,
    store: Arc<dyn ChannelSessionStore>,
}

impl ChannelSessionResolver {
    pub fn new(sessions: Arc<SessionManager>, store: Arc<dyn ChannelSessionStore>) -> Self {
        Self { sessions, store }
    }

    /// Return the aura `session_id` for this `(channel_type, user_id)`
    /// pair, creating (and persisting) a fresh one via
    /// [`SessionManager::create_session`] on a cache miss.
    pub async fn resolve_or_create(
        &self,
        channel_type: &ChannelType,
        user_id: &str,
    ) -> Result<String, ResolverError> {
        if let Some(existing) = self
            .store
            .get(channel_type, user_id)
            .await
            .map_err(|e| ResolverError::Store(e.to_string()))?
        {
            return Ok(existing);
        }

        let user = User {
            id: user_id.to_owned(),
            name: None,
            channel: channel_type.clone(),
            bot_id: None,
        };
        let session = self
            .sessions
            .create_session(user, channel_type.clone())
            .await
            .map_err(|e| ResolverError::SessionManager(e.to_string()))?;

        // Persist mapping. `put` survives a race where a second
        // concurrent inbound already inserted a mapping — the existing
        // one wins (see `LibsqlChannelSessionStore::put`), so a re-read
        // here is what the caller should trust.
        self.store
            .put(channel_type, user_id, &session.id)
            .await
            .map_err(|e| ResolverError::Store(e.to_string()))?;

        // Re-read so the caller gets the canonical winner even on a
        // tight race with another resolver.
        let canonical = self
            .store
            .get(channel_type, user_id)
            .await
            .map_err(|e| ResolverError::Store(e.to_string()))?
            .unwrap_or(session.id);
        Ok(canonical)
    }

    /// Drop the existing `(channel_type, user_id)` mapping (if any) and
    /// allocate a brand-new aura session for the same user. Returns the
    /// new `session_id`. The previous session row stays soft-deleted in
    /// the `sessions` table for replay/audit; only the channel mapping
    /// is repointed.
    ///
    /// `put`'s `ON CONFLICT` keeps the live `session_id` when a row is
    /// already live, so a delete is required to actually repoint.
    pub async fn reset_session(
        &self,
        channel_type: &ChannelType,
        user_id: &str,
    ) -> Result<String, ResolverError> {
        self.store
            .delete(channel_type, user_id)
            .await
            .map_err(|e| ResolverError::Store(e.to_string()))?;

        let user = User {
            id: user_id.to_owned(),
            name: None,
            channel: channel_type.clone(),
            bot_id: None,
        };
        let session = self
            .sessions
            .create_session(user, channel_type.clone())
            .await
            .map_err(|e| ResolverError::SessionManager(e.to_string()))?;

        self.store
            .put(channel_type, user_id, &session.id)
            .await
            .map_err(|e| ResolverError::Store(e.to_string()))?;

        tracing::info!(
            %channel_type,
            user_id_hash = %super::short_hash(user_id),
            session_id = %session.id,
            "channel mapping repointed to fresh session (/new)",
        );
        Ok(session.id)
    }
}

#[cfg(test)]
mod tests {
    use aura_storage::libsql::{LibsqlChannelSessionStore, LibsqlPool, LibsqlSessionStore};

    use super::*;

    async fn build() -> ChannelSessionResolver {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let session_store = Arc::new(LibsqlSessionStore::new(pool.clone()));
        let session_mgr = Arc::new(SessionManager::new(
            session_store,
            chrono::Duration::seconds(300),
        ));
        let channel_store = Arc::new(LibsqlChannelSessionStore::new(pool));
        ChannelSessionResolver::new(session_mgr, channel_store)
    }

    #[tokio::test]
    async fn resolves_same_session_id_for_repeat_user() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let first = resolver.resolve_or_create(&ct, "tg_42").await.unwrap();
        let second = resolver.resolve_or_create(&ct, "tg_42").await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn different_users_get_different_sessions() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let a = resolver.resolve_or_create(&ct, "tg_1").await.unwrap();
        let b = resolver.resolve_or_create(&ct, "tg_2").await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn same_user_across_channel_types_gets_different_sessions() {
        let resolver = build().await;
        let a = resolver
            .resolve_or_create(&ChannelType::telegram(), "42")
            .await
            .unwrap();
        let b = resolver
            .resolve_or_create(&ChannelType::discord(), "42")
            .await
            .unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn reset_session_repoints_mapping_to_new_id() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let first = resolver.resolve_or_create(&ct, "tg_42").await.unwrap();
        let fresh = resolver.reset_session(&ct, "tg_42").await.unwrap();
        assert_ne!(first, fresh);
        // Subsequent resolves now land on the fresh session.
        let after = resolver.resolve_or_create(&ct, "tg_42").await.unwrap();
        assert_eq!(after, fresh);
    }

    #[tokio::test]
    async fn reset_session_works_without_prior_mapping() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let fresh = resolver.reset_session(&ct, "tg_new").await.unwrap();
        let resolved = resolver.resolve_or_create(&ct, "tg_new").await.unwrap();
        assert_eq!(resolved, fresh);
    }
}
