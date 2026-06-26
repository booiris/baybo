//! Persistence contract for **in-flight device-pairing slots**.
//!
//! A slot is the short-lived bridge between two processes: the CLI
//! (`baybo device pair`) that mints a code and renders the QR, and the running
//! gateway WS (`/v1/device/pair`) that the app connects to. They share libsql,
//! not memory, so the minted code must be persisted — the gateway looks the
//! slot up by code to authorize the SPAKE2 handshake and learn the owning
//! `user_id` / `label`.
//!
//! A slot carries no key material — it only authorizes a handshake and then
//! brokers the live mutual-confirm. When SPAKE2 + `DeviceHello` complete the
//! gateway records the confirmation code + device id on the slot
//! ([`set_confirm`](DevicePairingStore::set_confirm)); the operator's live
//! `baybo device pair` polls that, shows the code, and writes its decision
//! ([`set_operator_decision`](DevicePairingStore::set_operator_decision)); the
//! gateway then writes a durable [`crate::device::DeviceRow`] and deletes the
//! slot. Unconsumed slots age out via [`DevicePairingStore::purge_expired`].
//! This mirrors [`crate::channel_pairing::ChannelPairingStore`] but is
//! single-use and keyed solely by `code`.

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
    /// The human-comparable confirmation code both ends display, set by the
    /// gateway once SPAKE2 + `DeviceHello` complete. `None` until then. Derived
    /// from the SPAKE2 secret — not itself secret.
    pub confirm_code: Option<String>,
    /// The app-generated device id of the phone in the live handshake, recorded
    /// alongside `confirm_code` so the operator's `device pair` can name it.
    pub device_id: Option<String>,
    /// The operator's confirm decision: `Some(true)` approve, `Some(false)`
    /// decline, `None` undecided. Written by `baybo device pair`; the gateway
    /// polls it before sealing the welcome.
    pub operator_decision: Option<bool>,
    /// The phone-side outcome, written by the gateway when it abandons the
    /// confirm step for a device-side reason — the phone user declined, or the
    /// app dropped before deciding. `Some(false)` = the device will not pair;
    /// `None` = still deciding. Symmetric with [`operator_decision`] but in the
    /// other direction: it lets the operator's `baybo device pair` stop waiting
    /// the instant the phone backs out, instead of sitting until its own
    /// timeout. The gateway never records `Some(true)` here — a successful pair
    /// is observed via the approved [`crate::device::DeviceRow`].
    ///
    /// [`operator_decision`]: Self::operator_decision
    pub device_decision: Option<bool>,
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

    /// Atomically delete a slot by code, returning whether a row was actually
    /// removed. This is the single-use **consume** on handshake success: two
    /// clients that scanned the same live code race here, and only the one that
    /// gets `true` may finalize a device — so one code mints at most one device.
    async fn delete_slot(&self, code: &str) -> Result<bool>;

    /// Publish the confirm challenge: the gateway, after SPAKE2 + `DeviceHello`,
    /// records the confirmation code + the live device id + the resolved device
    /// label so the operator's `baybo device pair` can display them. The label
    /// is the device's self-reported name (or the operator's override). No-op if
    /// the slot is gone.
    async fn set_confirm(
        &self,
        code: &str,
        confirm_code: &str,
        device_id: &str,
        label: &str,
    ) -> Result<()>;

    /// Record the operator's confirm decision (written by `baybo device pair`).
    /// The gateway polls [`get_slot`](Self::get_slot) for it before sealing the
    /// welcome. No-op if the slot is gone.
    async fn set_operator_decision(&self, code: &str, accepted: bool) -> Result<()>;

    /// Record the phone-side outcome (written by the gateway when the phone
    /// declines or drops during the confirm step). The operator's `baybo device
    /// pair` polls [`get_slot`](Self::get_slot) for it so it can stop waiting
    /// the moment the phone backs out. No-op if the slot is gone.
    async fn set_device_decision(&self, code: &str, accepted: bool) -> Result<()>;

    /// List all slots, newest `created_at` first (CLI visibility).
    async fn list_slots(&self) -> Result<Vec<DevicePairingSlot>>;

    /// Delete every slot whose `expires_at <= now`. Returns the count removed.
    async fn purge_expired(&self, now: i64) -> Result<u64>;
}
