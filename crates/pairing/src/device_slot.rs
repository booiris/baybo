//! The in-flight device-pairing **slot** DTO.
//!
//! A slot is the short-lived, in-process bookkeeping for one live
//! `baybo device pair` run: it carries the public `rendezvous_id`, the QR
//! `secret` (the Noise PSK), the confirmation code both ends compare once the
//! handshake completes, and each side's confirm decision. It is held in memory
//! by [`DevicePairingService`](crate::DevicePairingService) for the lifetime of
//! the command — pairing is driven entirely by that single interactive process
//! (the operator's CLI hosts the relay leg *and* runs the handshake), so the
//! slot never needs to be durable or shared across processes.
//!
//! ## Two fields, opposite handling
//!
//! - `rendezvous_id` is **public**: the relay sees it (it routes on it), and it
//!   is the only pairing identifier that ever lands in a durable row, a log, or
//!   `device list`.
//! - `secret` is a **credential**: the Noise PSK that authenticates the
//!   handshake against a malicious relay. It travels *only* in the QR and lives
//!   *only* here, in memory, for the run — never in a plaintext column, never in
//!   the durable [`DeviceRow`](baybo_store::DeviceRow), never logged, and
//!   zeroized on drop ([`device_proto::psk_pair::PairingSecret`]). Keeping it in
//!   this in-memory single-use slot (rather than a durable encrypted vault)
//!   keeps it out of every persisted store entirely — strictly stronger than the
//!   doc's "vault it" sketch, and possible because mint + handshake share one
//!   process.

use device_proto::psk_pair::PairingSecret;

/// One in-flight pairing slot. Keyed by `rendezvous_id` inside the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePairingSlot {
    /// The public rendezvous id (a UUID): the relay route param, the broker
    /// key, and the slot lookup key. Not secret.
    pub rendezvous_id: String,
    /// The QR secret used as the Noise PSK. Credential material — see the module
    /// docs; never persisted or logged, zeroized on drop.
    pub secret: PairingSecret,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; the slot is dead once `now >= expires_at`.
    pub expires_at: i64,
    /// The human-comparable confirmation code both ends display, set once the
    /// handshake completes and `DeviceHello` is read. `None` until then. Derived
    /// from the Noise handshake hash `h` — not itself secret.
    pub confirm_code: Option<String>,
    /// The app-generated device id of the phone in the live handshake, recorded
    /// alongside `confirm_code` so the operator's `baybo device pair` can name it.
    pub device_id: Option<String>,
    /// The operator's confirm decision: `Some(true)` approve, `Some(false)`
    /// decline, `None` undecided. Written by `baybo device pair`; the handshake
    /// observes it before sealing the welcome.
    pub operator_decision: Option<bool>,
    /// The phone-side outcome, set when the handshake abandons the confirm step
    /// for a device-side reason — the phone user declined, or the app dropped
    /// before deciding. `Some(false)` = the device will not pair; `None` = still
    /// deciding. Symmetric with [`operator_decision`](Self::operator_decision)
    /// but in the other direction: it lets the operator's `baybo device pair`
    /// stop waiting the instant the phone backs out. Never `Some(true)` — a
    /// successful pair is observed via the approved
    /// [`DeviceRow`](baybo_store::DeviceRow).
    pub device_decision: Option<bool>,
}

impl DevicePairingSlot {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}
