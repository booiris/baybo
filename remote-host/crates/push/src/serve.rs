//! Wiring the push role into a runnable service: [`PushConfig`] + the
//! [`build_router`] that assembles the signer, admission allow-list, device
//! store, the live [`HttpApnsSender`], and the `/notify` + `/register` routes.
//! `main.rs` loads the config from the environment, reads the `.p8`, and serves
//! the returned router; `build_router` is host-tested with a throwaway key.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;

use crate::apns_http::HttpApnsSender;
use crate::error::PushError;
use crate::http::{PushState, router};
use crate::jwt::ApnsProviderToken;
use crate::notify::NotifyService;
use crate::store::{InMemoryAdmission, InMemoryDeviceTokenStore};

/// Router config for the push role.
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// APNs auth-key id (the `.p8`'s Key ID).
    pub key_id: String,
    /// Apple Developer Team ID (the provider-token `iss`).
    pub team_id: String,
    /// `apns-topic` — the published app's bundle id.
    pub topic: String,
    /// The admitted gateway instance keys (the admission allow-list).
    pub instance_keys: Vec<String>,
}

impl PushConfig {
    /// Load the config + `.p8` path from the environment. Required:
    /// `APNS_P8_PATH`, `APNS_KEY_ID`, `APNS_TEAM_ID`, `APNS_BUNDLE_ID`,
    /// `PUSH_INSTANCE_KEYS` (comma-separated).
    pub fn from_env() -> Result<(Self, PathBuf), PushError> {
        fn req(key: &str) -> Result<String, PushError> {
            std::env::var(key).map_err(|_| PushError::Config(format!("missing env {key}")))
        }
        let p8_path = PathBuf::from(req("APNS_P8_PATH")?);
        let instance_keys: Vec<String> = req("PUSH_INSTANCE_KEYS")?
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if instance_keys.is_empty() {
            return Err(PushError::Config(
                "PUSH_INSTANCE_KEYS must list at least one admitted gateway key".into(),
            ));
        }
        let config = PushConfig {
            key_id: req("APNS_KEY_ID")?,
            team_id: req("APNS_TEAM_ID")?,
            topic: req("APNS_BUNDLE_ID")?,
            instance_keys,
        };
        Ok((config, p8_path))
    }
}

/// Assemble the push router from config + the `.p8` PEM bytes. The
/// [`HttpApnsSender`] is the live APNs transport; the device store is in-memory
/// (devices re-register on reconnect, so it survives restart by re-population).
pub fn build_router(config: &PushConfig, p8_pem: &[u8]) -> Result<Router, PushError> {
    let signer = Arc::new(ApnsProviderToken::new(
        config.key_id.clone(),
        config.team_id.clone(),
        p8_pem,
    )?);
    let admission = Arc::new(InMemoryAdmission::with_keys(config.instance_keys.clone()));
    let store = Arc::new(InMemoryDeviceTokenStore::new());
    let sender = Arc::new(HttpApnsSender::new());
    let service = Arc::new(NotifyService::new(
        admission,
        store,
        sender,
        signer,
        config.topic.clone(),
    ));
    Ok(router(PushState { service }))
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
            instance_keys: vec!["inst-A".into()],
        }
    }

    #[test]
    fn build_router_succeeds_with_a_valid_key() {
        assert!(build_router(&config(), TEST_P8.as_bytes()).is_ok());
    }

    #[test]
    fn build_router_rejects_a_bad_key() {
        let err = build_router(&config(), b"not a pem").unwrap_err();
        assert!(matches!(err, PushError::Key(_)));
    }
}
