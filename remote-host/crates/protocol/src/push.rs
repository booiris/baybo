//! Push (HTTP) wire surface: the blind APNs notify + device registration.

use serde::{Deserialize, Serialize};

/// Route paths.
pub const NOTIFY: &str = "/notify";
pub const REGISTER: &str = "/register";

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
/// so a tenant sharing the `remote_api_key` (e.g. the `guest` trial key) cannot
/// spam or replay another device's notifications even if it learns the
/// `device_id`. See `device-proto`'s `delegation` module for the signed byte
/// layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub remote_api_key: String,
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

/// JSON body of `POST /register` — bind/rebind a device's APNs token. The caller
/// presents the relay admission key for admission, but the binding is owned by
/// the device's Ed25519 identity (`device_id == ios-<hex(device_pubkey)>`): the
/// device delegates a gateway push key, and the gateway signs the binding. C
/// verifies the chain with no stored secret, so under a shared `remote_api_key`
/// one tenant cannot overwrite, redirect, or suppress another's binding. APNs
/// provider credentials stay on C.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub remote_api_key: String,
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
