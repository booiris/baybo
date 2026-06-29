//! Wiring the push role into a runnable service: [`PushConfig`] + the
//! [`build_router`] that assembles the signer, device store, the live
//! [`HttpApnsSender`], and the `/notify` + `/register` routes. `main.rs` loads the
//! config from the environment, reads the `.p8`, and serves the returned router;
//! `build_router` is host-tested with a throwaway key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use remote_host_ratelimit::ip_limit::{self, IpLimitConfig};

use crate::apns_http::HttpApnsSender;
use crate::error::PushError;
use crate::http::{PushState, router};
use crate::jwt::ApnsProviderToken;
use crate::notify::NotifyService;
use crate::ratelimit::NotifyRateLimiter;
use crate::store::InMemoryDeviceTokenStore;

/// Operator-tunable push limits, each overridable from the environment; an
/// unset/unparseable/non-positive value falls back to the built-in const default.
#[derive(Debug, Clone)]
pub struct PushLimits {
    /// Device-token store soft cap (`PUSH_DEVICE_STORE_CAP`).
    pub device_store_cap: usize,
    /// TTL for bindings never confirmed by a `/notify` (`PUSH_UNCONFIRMED_TTL_SECS`).
    pub unconfirmed_ttl: Duration,
    /// Per-device sustained `/notify` rate, pushes/min (`PUSH_NOTIFY_RATE_PER_MIN`).
    pub notify_rate_per_min: f64,
    /// Per-device `/notify` burst (`PUSH_NOTIFY_BURST`).
    pub notify_burst: f64,
}

impl Default for PushLimits {
    fn default() -> Self {
        Self {
            device_store_cap: crate::store::DEVICE_STORE_SOFT_CAP,
            unconfirmed_ttl: crate::store::UNCONFIRMED_TTL,
            notify_rate_per_min: crate::ratelimit::NOTIFY_RATE_PER_MIN,
            notify_burst: crate::ratelimit::NOTIFY_BURST,
        }
    }
}

impl PushLimits {
    /// Read overrides from the environment, each falling back to its const default.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            device_store_cap: env_usize("PUSH_DEVICE_STORE_CAP", d.device_store_cap),
            unconfirmed_ttl: env_secs("PUSH_UNCONFIRMED_TTL_SECS", d.unconfirmed_ttl),
            notify_rate_per_min: env_f64("PUSH_NOTIFY_RATE_PER_MIN", d.notify_rate_per_min),
            notify_burst: env_f64("PUSH_NOTIFY_BURST", d.notify_burst),
        }
    }
}

/// Parse a positive `usize` env override, else `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

/// Parse a positive `f64` env override, else `default`.
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&v| v > 0.0)
        .unwrap_or(default)
}

/// Parse a positive whole-seconds env override into a `Duration`, else `default`.
fn env_secs(key: &str, default: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// Router config for the push role.
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// APNs auth-key id (the `.p8`'s Key ID).
    pub key_id: String,
    /// Apple Developer Team ID (the provider-token `iss`).
    pub team_id: String,
    /// `apns-topic` — the published app's bundle id.
    pub topic: String,
    /// Operator-tunable global limits (env-overridable; see [`PushLimits`]).
    pub limits: PushLimits,
}

impl PushConfig {
    /// Load the config + `.p8` path from the environment. Required:
    /// `APNS_P8_PATH`, `APNS_KEY_ID`, `APNS_TEAM_ID`, `APNS_BUNDLE_ID`. The tunable
    /// limits (`PUSH_*`) are optional and default to the built-in consts.
    pub fn from_env() -> Result<(Self, PathBuf), PushError> {
        fn req(key: &str) -> Result<String, PushError> {
            std::env::var(key).map_err(|_| PushError::Config(format!("missing env {key}")))
        }
        let p8_path = PathBuf::from(req("APNS_P8_PATH")?);
        let config = PushConfig {
            key_id: req("APNS_KEY_ID")?,
            team_id: req("APNS_TEAM_ID")?,
            topic: req("APNS_BUNDLE_ID")?,
            limits: PushLimits::from_env(),
        };
        Ok((config, p8_path))
    }
}

/// Assemble the push router from config + the `.p8` PEM bytes. The
/// [`HttpApnsSender`] is the live APNs transport; the device store is in-memory
/// (devices re-register on reconnect, so it survives restart by re-population).
///
/// `ip_limit` mounts the shared per-source-IP request throttle ahead of body
/// parsing / signature verification, so a `/register` or `/notify` flood is shed
/// with `429` by client IP before any Ed25519 work. It uses the same proxy-aware
/// client-IP resolution as the relay (see [`remote_host_ratelimit::ip_limit`]).
pub fn build_router(
    config: &PushConfig,
    p8_pem: &[u8],
    ip_limit: IpLimitConfig,
) -> Result<Router, PushError> {
    let signer = Arc::new(ApnsProviderToken::new(
        config.key_id.clone(),
        config.team_id.clone(),
        p8_pem,
    )?);
    let store = Arc::new(InMemoryDeviceTokenStore::with_limits(
        config.limits.device_store_cap,
        config.limits.unconfirmed_ttl,
    ));
    let sender = Arc::new(HttpApnsSender::new());
    let service = Arc::new(NotifyService::new(
        store,
        sender,
        signer,
        config.topic.clone(),
        NotifyRateLimiter::for_store_cap(
            config.limits.notify_rate_per_min,
            config.limits.notify_burst,
            config.limits.device_store_cap,
        ),
    ));
    Ok(ip_limit::apply(router(PushState { service }), ip_limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Throwaway P-256 key (NOT an APNs key) — same fixture style as jwt tests.
    const TEST_P8: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPFauT/kbqwIxcoQW
BNxFLAfYXAa3OFmTIx3IcGqjUkyhRANCAATGtaYrLt8AL8cs25DIa+OeV4PCpUHt
SYW9s/UKX8shed4rIxRqMe3POJIY7OsF06EEtnyLrMjJg53H5HWAe2Mh
-----END PRIVATE KEY-----"#;

    fn config() -> PushConfig {
        PushConfig {
            key_id: "KEY123".into(),
            team_id: "TEAM456".into(),
            topic: "com.baybo.app".into(),
            limits: PushLimits::default(),
        }
    }

    #[test]
    fn build_router_succeeds_with_a_valid_key() {
        assert!(build_router(&config(), TEST_P8.as_bytes(), IpLimitConfig::disabled()).is_ok());
    }

    #[test]
    fn build_router_rejects_a_bad_key() {
        let err = build_router(&config(), b"not a pem", IpLimitConfig::disabled()).unwrap_err();
        assert!(matches!(err, PushError::Key(_)));
    }

    #[test]
    fn push_limits_default_to_the_consts() {
        let d = PushLimits::default();
        assert_eq!(d.device_store_cap, crate::store::DEVICE_STORE_SOFT_CAP);
        assert_eq!(d.unconfirmed_ttl, crate::store::UNCONFIRMED_TTL);
        assert_eq!(d.notify_rate_per_min, crate::ratelimit::NOTIFY_RATE_PER_MIN);
        assert_eq!(d.notify_burst, crate::ratelimit::NOTIFY_BURST);
    }

    /// The env overrides are read and validated: a positive value wins, an unset /
    /// unparseable / non-positive one falls back to the default. Uses a unique key
    /// so it can't race another test reading the same var.
    #[test]
    fn env_overrides_are_read_and_validated() {
        const KEY: &str = "PUSH_TEST_CAP_OVERRIDE_UNIQUE";
        // SAFETY: a test-only unique key no other test reads; cleaned up below.
        unsafe { std::env::set_var(KEY, "123") };
        assert_eq!(env_usize(KEY, 42), 123, "a positive override wins");
        unsafe { std::env::set_var(KEY, "0") };
        assert_eq!(env_usize(KEY, 42), 42, "non-positive is ignored");
        unsafe { std::env::set_var(KEY, "nope") };
        assert_eq!(env_usize(KEY, 42), 42, "unparseable is ignored");
        unsafe { std::env::remove_var(KEY) };
        assert_eq!(env_usize(KEY, 42), 42, "unset falls back to the default");
        assert_eq!(
            env_secs(KEY, Duration::from_secs(7)),
            Duration::from_secs(7)
        );
        assert_eq!(env_f64(KEY, 1.5), 1.5);
    }

    /// F2: a `/register` flood from one client IP is shed with `429` by the shared
    /// per-IP layer once the burst is spent — *ahead* of body parsing (a malformed
    /// body would otherwise 422), proving the throttle runs before any per-request
    /// work. Keyed on the trusted `cf-connecting-ip` header so the test drives it
    /// over a single in-process service.
    #[tokio::test]
    async fn push_routes_are_ip_throttled_before_body_parsing() {
        use axum::body::Body;
        use axum::http::{HeaderName, StatusCode, header};
        use remote_host_ratelimit::ip_limit::IP_BURST;
        use tower::ServiceExt;

        let app = build_router(
            &config(),
            TEST_P8.as_bytes(),
            IpLimitConfig::with_trusted_headers(vec![HeaderName::from_static("cf-connecting-ip")]),
        )
        .unwrap();

        let post = || {
            axum::http::Request::builder()
                .method("POST")
                .uri("/register")
                .header(header::CONTENT_TYPE, "application/json")
                .header("cf-connecting-ip", "203.0.113.5")
                // Deliberately not a valid RegisterRequest: a request that clears the
                // IP layer reaches the handler and is rejected there (non-429); once
                // the burst is spent the IP layer 429s before parsing this at all.
                .body(Body::from("{}"))
                .unwrap()
        };

        let mut saw_non_429 = false;
        let mut saw_429 = false;
        for _ in 0..(IP_BURST as usize + 8) {
            let status = app.clone().oneshot(post()).await.unwrap().status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                saw_429 = true;
                break;
            }
            saw_non_429 = true;
        }
        assert!(saw_non_429, "early requests pass the IP limiter");
        assert!(
            saw_429,
            "the per-IP burst is exhausted and shed with 429 by client IP"
        );
    }
}
