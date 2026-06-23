//! The APNs send seam.
//!
//! The push role builds a blind APNs request — the operator-encrypted preview
//! (`enc`/`n`/`kid`/`bid`) copied verbatim into the payload, plus a signed
//! provider token — and hands it to an [`ApnsSender`]. The trait keeps the real
//! HTTP/2 transport (driven against `api.push.apple.com` only on a real device,
//! M4) behind a seam so the whole `/notify` path is host-testable against a
//! mock. This component never decrypts `enc` — it stays blind.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Which APNs environment a device token is bound to. A sandbox token is
/// rejected by the production host and vice versa, so it is tracked per device
/// (the #1 APNs footgun) and never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApnsEnv {
    Sandbox,
    Production,
}

impl ApnsEnv {
    /// The APNs API host for this environment (the same `.p8` serves both —
    /// only the host differs).
    pub fn host(self) -> &'static str {
        match self {
            ApnsEnv::Sandbox => "api.sandbox.push.apple.com",
            ApnsEnv::Production => "api.push.apple.com",
        }
    }
}

/// One fully-built APNs request. Constructed blind by the push role; the sender
/// just transmits it.
#[derive(Debug, Clone)]
pub struct ApnsRequest {
    pub env: ApnsEnv,
    /// The device's APNs token (the path component of the APNs URL).
    pub device_token: String,
    /// `apns-topic` — the published app's bundle id.
    pub topic: String,
    /// `apns-collapse-id` — `bid:session_id` so multi-gateway pushes don't
    /// coalesce.
    pub collapse_id: String,
    /// `authorization: bearer <jwt>` — the ES256 provider token.
    pub provider_jwt: String,
    /// The JSON payload body (`aps` + the verbatim `enc`/`n`/`kid`/`bid`).
    pub payload: Vec<u8>,
}

/// Normalized outcome of an APNs send across transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApnsOutcome {
    /// 200 — accepted by APNs.
    Delivered,
    /// 400 `BadDeviceToken` — unbind the token. (P3 distinguishes a genuine
    /// dead token from an env-mismatch 400; here both prune.)
    BadDeviceToken,
    /// 410 `Unregistered` — unbind the token, honoring `timestamp_ms` (the
    /// instant the token became invalid) before deleting.
    Unregistered { timestamp_ms: u64 },
    /// Any other / transport failure — retryable, do not prune.
    TransientError(String),
}

/// The send seam. The real impl POSTs to `env.host()` over HTTP/2; tests use a
/// mock.
#[async_trait]
pub trait ApnsSender: Send + Sync {
    async fn send(&self, req: ApnsRequest) -> ApnsOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_hosts_are_distinct() {
        assert_eq!(ApnsEnv::Sandbox.host(), "api.sandbox.push.apple.com");
        assert_eq!(ApnsEnv::Production.host(), "api.push.apple.com");
    }
}
