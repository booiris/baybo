//! Push (HTTP) wire surface: the blind APNs notify + device registration.

use serde::{Deserialize, Serialize};

/// Route paths.
pub const NOTIFY: &str = "/notify";
pub const REGISTER: &str = "/register";

/// Ed25519 domain-separation contexts prefixing each signed message in the
/// push-binding chain, distinct per kind so a signature minted for one role can
/// never verify as another. The gateway/app (`device-proto`) sign; the host
/// (`remote-host-push`) verifies — two implementations in workspaces that cannot
/// link each other. Both consume these consts, so the context bytes are a single
/// source of truth and cannot silently drift and break every push. The rest of
/// the signed byte layout (the `env` mapping and the `u32`-LE length-prefix
/// framing) lives in each `delegation` module, guarded against drift by their
/// shared pinned-signature cross-impl test vector.
pub const DELEGATION_CONTEXT: &[u8] = b"baybo/push/delegation/v1";
pub const REGISTER_CONTEXT: &[u8] = b"baybo/push/register/v1";
pub const NOTIFY_CONTEXT: &[u8] = b"baybo/push/notify/v1";

/// Which APNs environment a device token is bound to (the `env` of
/// [`RegisterRequest`]). A sandbox token is rejected by the production host and
/// vice versa, so it is tracked per device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApnsEnv {
    Sandbox,
    Production,
}

/// JSON body of `POST /notify` — the gateway's blind, operator-encrypted preview
/// for one pushable turn. `enc`/`n` are opaque base64 (copied verbatim into the
/// APNs payload); C never decrypts them.
///
/// `sig`/`counter` authenticate the notify under the gateway push key the device
/// delegated to at `/register` (C verifies against the stored `gateway_pubkey`),
/// so no one can spam or replay another device's notifications even if it learns
/// the `device_id` — the binding's delegation chain, not a caller key, is the
/// guard. See `device-proto`'s `delegation` module for the signed byte layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub device_id: String,
    pub collapse_id: String,
    pub bid: String,
    pub enc: String,
    pub n: String,
    /// base64 Ed25519 signature (64B) over this notify by the delegated gateway
    /// push key.
    pub sig: String,
    /// Strictly-increasing per-`device_id` replay counter (C rejects a notify
    /// whose counter does not exceed the last accepted one).
    pub counter: u64,
}

/// JSON body of `POST /register` — bind/rebind a device's APNs token. The binding
/// is owned by the device's Ed25519 identity (`device_id == ios-<hex(device_pubkey)>`):
/// the device delegates a gateway push key, and the gateway signs the binding. C
/// verifies the chain with no stored secret, so no one can overwrite, redirect, or
/// suppress another's binding even without a caller key. APNs provider credentials
/// stay on C.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub device_id: String,
    pub apns_token: String,
    pub env: ApnsEnv,
    /// base64 gateway Ed25519 push pubkey (32B) the device authorized in
    /// `delegation`. C stores it and verifies later `/notify` signatures against it.
    pub gateway_pubkey: String,
    /// base64 device→gateway delegation signature (64B), signed by the device
    /// identity key whose public half is encoded in `device_id`.
    pub delegation: String,
    /// base64 Ed25519 signature (64B) over this register by `gateway_pubkey`.
    pub sig: String,
    /// Strictly-increasing per-`device_id` replay counter.
    pub counter: u64,
}

/// `{base}/notify`.
pub fn notify_url(base: &str) -> String {
    crate::join(base, NOTIFY)
}
/// `{base}/register`.
pub fn register_url(base: &str) -> String {
    crate::join(base, REGISTER)
}
