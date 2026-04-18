//! Persistent input-history backend for the TUI.
//!
//! The trait is deliberately minimal — the TUI only needs to load prior
//! submissions at startup and persist the updated list after each one.
//! Implementations decide where the bytes live; in production Aura wraps
//! the `SecretVault` so inputs that contain API keys or other secrets
//! stay encrypted at rest.

use async_trait::async_trait;

/// Store for persisting the TUI input-history ring across sessions.
///
/// Treated as opaque from the adapter's perspective: the TUI hands
/// `Vec<String>` in and gets `Vec<String>` back, leaving encryption,
/// on-disk layout, and error handling to the implementation.
#[async_trait]
pub trait InputHistoryStore: Send + Sync {
    /// Return the persisted history in chronological order (oldest first).
    /// Return `Ok(vec![])` when no history has been saved yet.
    async fn load(&self) -> anyhow::Result<Vec<String>>;

    /// Replace the persisted history with the supplied snapshot. The TUI
    /// passes the full ring every time — implementations do not need to
    /// diff against prior state.
    async fn save(&self, history: &[String]) -> anyhow::Result<()>;
}
