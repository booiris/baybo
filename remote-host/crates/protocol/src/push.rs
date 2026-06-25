//! Push (HTTP) wire surface: the blind APNs notify + device registration.

use serde::{Deserialize, Serialize};

/// Route paths.
pub const NOTIFY: &str = "/notify";
pub const REGISTER: &str = "/register";

/// Which APNs environment a device token is bound to (the `env` of
/// [`RegisterRequest`]). A sandbox token is rejected by the production host and
/// vice versa, so it is tracked per device.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApnsEnv {
    Sandbox,
    Production,
}

/// JSON body of `POST /notify` — the gateway's blind, operator-encrypted preview
/// for one pushable turn. `enc`/`n` are opaque base64 (copied verbatim into the
/// APNs payload); C never decrypts them.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub instance_key: String,
    pub device_id: String,
    pub collapse_id: String,
    pub kid: u32,
    pub bid: String,
    pub enc: String,
    pub n: String,
}

/// JSON body of `POST /register` — bind/rebind a device's APNs token
/// (gateway-mediated, so the app never holds a C credential).
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub instance_key: String,
    pub device_id: String,
    pub apns_token: String,
    pub env: ApnsEnv,
}

/// `{base}/notify`.
pub fn notify_url(base: &str) -> String {
    crate::join(base, NOTIFY)
}
/// `{base}/register`.
pub fn register_url(base: &str) -> String {
    crate::join(base, REGISTER)
}
