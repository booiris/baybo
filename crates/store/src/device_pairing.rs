//! Persistence contract for **in-flight device-pairing slots**.
//!
//! A slot is the short-lived bridge between two processes: the CLI
//! (`aura device pair`) that mints a code and renders the QR, and the running
//! gateway WS (`/v1/device/pair`) that the app connects to. They share libsql,
//! not memory, so the minted code must be persisted — the gateway looks the
//! slot up by code to authorize the SPAKE2 handshake and learn the owning
//! `user_id` / `label`.
//!
//! A slot carries no key material — it only authorizes a handshake. When the
//! handshake completes the gateway writes a durable [`crate::device::DeviceRow`]
//! (retaining the code as the approval handle) and deletes the slot. Unconsumed
//! slots age out via [`DevicePairingStore::purge_expired`]. This mirrors
//! [`crate::channel_pairing::ChannelPairingStore`] but is single-use and keyed
//! solely by `code`.

use async_trait::async_trait;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One pending pairing slot. Keyed by `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePairingSlot {
    pub code: String,
    pub user_id: String,
    pub label: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; the slot is dead once `now >= expires_at`.
    pub expires_at: i64,
}

impl DevicePairingSlot {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// Persistence contract for pairing slots.
#[async_trait]
pub trait DevicePairingStore: Send + Sync {
    /// Insert a fresh slot. Errors with [`StorageError::Conflict`] if `code`
    /// already exists (the caller mints with a uniqueness check, so this is
    /// only hit on a genuine race).
    async fn create_slot(&self, slot: &DevicePairingSlot) -> Result<()>;

    /// Fetch a slot by code. Returns expired slots too — the caller checks
    /// [`DevicePairingSlot::is_expired`] against its own `now`.
    async fn get_slot(&self, code: &str) -> Result<Option<DevicePairingSlot>>;

    /// Delete a slot by code (single-use consumption on handshake success, or
    /// operator cancel). No-op if already gone.
    async fn delete_slot(&self, code: &str) -> Result<()>;

    /// List all slots, newest `created_at` first (CLI visibility).
    async fn list_slots(&self) -> Result<Vec<DevicePairingSlot>>;

    /// Delete every slot whose `expires_at <= now`. Returns the count removed.
    async fn purge_expired(&self, now: i64) -> Result<u64>;
}
