//! The in-flight device-pairing **slot** DTO.
//!
//! A slot is the short-lived, in-process bookkeeping for one live
//! `baybo device pair` run: it carries the owning `user_id` / `label`, the
//! confirmation code both ends compare once SPAKE2 completes, and each side's
//! confirm decision. It is held in memory by
//! [`baybo_pairing::DevicePairingService`](../../baybo_pairing) for the lifetime
//! of the command — pairing is driven entirely by that single interactive
//! process (the operator's CLI hosts the relay leg *and* runs the handshake), so
//! the slot never needs to be durable or shared across processes. It carries no
//! key material; the only durable artefact a pairing produces is the approved
//! [`crate::device::DeviceRow`].

/// One in-flight pairing slot. Keyed by `code` inside the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePairingSlot {
    pub code: String,
    pub user_id: String,
    pub label: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; the slot is dead once `now >= expires_at`.
    pub expires_at: i64,
    /// The human-comparable confirmation code both ends display, set once the
    /// SPAKE2 handshake and `DeviceHello` complete. `None` until then. Derived
    /// from the SPAKE2 secret — not itself secret.
    pub confirm_code: Option<String>,
    /// The app-generated device id of the phone in the live handshake, recorded
    /// alongside `confirm_code` so the operator's `device pair` can name it.
    pub device_id: Option<String>,
    /// The operator's confirm decision: `Some(true)` approve, `Some(false)`
    /// decline, `None` undecided. Written by `baybo device pair`; the handshake
    /// observes it before sealing the welcome.
    pub operator_decision: Option<bool>,
    /// The phone-side outcome, set when the handshake abandons the confirm step
    /// for a device-side reason — the phone user declined, or the app dropped
    /// before deciding. `Some(false)` = the device will not pair; `None` = still
    /// deciding. Symmetric with [`operator_decision`] but in the other direction:
    /// it lets the operator's `baybo device pair` stop waiting the instant the
    /// phone backs out. Never `Some(true)` — a successful pair is observed via the
    /// approved [`crate::device::DeviceRow`].
    ///
    /// [`operator_decision`]: Self::operator_decision
    pub device_decision: Option<bool>,
}

impl DevicePairingSlot {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}
