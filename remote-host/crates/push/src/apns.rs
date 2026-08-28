//! APNs provider adapter.
//!
//! The shared push pipeline hands encrypted, provider-independent content to
//! [`ApnsProvider`]. This module alone owns APNs payload framing, provider-token
//! signing, environment routing, and response classification.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;

use crate::jwt::ApnsProviderToken;
use crate::provider::{EncryptedPush, ProviderDelivery, ProviderOutcome, ProviderSender};
use remote_host_protocol::push::{PushProvider, PushTarget};

pub use remote_host_protocol::push::ApnsEnvironment;

/// The APNs API host for an environment. The same `.p8` serves both; only the
/// host differs.
pub fn host(environment: ApnsEnvironment) -> &'static str {
    match environment {
        ApnsEnvironment::Sandbox => "api.sandbox.push.apple.com",
        ApnsEnvironment::Production => "api.push.apple.com",
    }
}

/// One fully-built APNs request. Constructed blind by [`ApnsProvider`]; the
/// sender only transmits it.
#[derive(Debug, Clone)]
pub struct ApnsRequest {
    pub environment: ApnsEnvironment,
    /// The device's APNs token (the path component of the APNs URL).
    pub device_token: String,
    /// `apns-topic` — the published app's bundle id.
    pub topic: String,
    /// `apns-collapse-id` — provider-specific rendering of the shared collapse
    /// key.
    pub collapse_id: String,
    /// `authorization: bearer <jwt>` — the ES256 provider token.
    pub provider_jwt: String,
    /// The JSON payload body (`aps` + the verbatim `enc`/`n`/`bid`).
    pub payload: Vec<u8>,
}

/// Normalized outcome of an APNs send across transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApnsOutcome {
    /// 200 — accepted by APNs. `apns_id` is Apple's per-notification trace id.
    Delivered { apns_id: Option<String> },
    /// 400 `BadDeviceToken` — unbind the token.
    BadDeviceToken,
    /// 410 `Unregistered` — unbind the token.
    Unregistered { timestamp_ms: u64 },
    /// Any other / transport failure — retryable, do not prune.
    TransientError(String),
}

/// The low-level APNs transport seam.
#[async_trait]
pub trait ApnsSender: Send + Sync {
    async fn send(&self, req: ApnsRequest) -> ApnsOutcome;
}

/// APNs implementation of the provider-neutral send seam.
pub struct ApnsProvider {
    sender: Arc<dyn ApnsSender>,
    signer: Arc<ApnsProviderToken>,
    cached_jwt: Mutex<Option<(String, u64)>>,
    topic: String,
}

impl ApnsProvider {
    pub fn new(
        sender: Arc<dyn ApnsSender>,
        signer: Arc<ApnsProviderToken>,
        topic: impl Into<String>,
    ) -> Self {
        Self {
            sender,
            signer,
            cached_jwt: Mutex::new(None),
            topic: topic.into(),
        }
    }

    fn provider_jwt(&self, now: u64) -> Result<String, String> {
        if let Some((jwt, issued_at)) = self.cached_jwt.lock().as_ref()
            && !ApnsProviderToken::needs_refresh(*issued_at, now)
        {
            return Ok(jwt.clone());
        }
        let jwt = self.signer.sign(now).map_err(|error| error.to_string())?;
        tracing::debug!(iat = now, "push: APNs provider JWT re-signed");
        *self.cached_jwt.lock() = Some((jwt.clone(), now));
        Ok(jwt)
    }

    fn payload(message: &EncryptedPush) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&json!({
            "aps": {
                "alert": { "title": "Baybo", "body": "New message" },
                "mutable-content": 1,
            },
            "enc": message.enc,
            "n": message.n,
            "bid": message.bid,
        }))
        .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl ProviderSender for ApnsProvider {
    fn provider(&self) -> PushProvider {
        PushProvider::Apns
    }

    async fn send(
        &self,
        target: &PushTarget,
        message: EncryptedPush,
        now: u64,
    ) -> ProviderDelivery {
        let PushTarget::Apns { token, environment } = target else {
            return ProviderDelivery {
                payload_len: 0,
                outcome: ProviderOutcome::TransientError(
                    "APNs adapter received a non-APNs target".into(),
                ),
            };
        };
        let payload = match Self::payload(&message) {
            Ok(payload) => payload,
            Err(error) => {
                return ProviderDelivery {
                    payload_len: 0,
                    outcome: ProviderOutcome::TransientError(format!(
                        "APNs payload serialization failed: {error}"
                    )),
                };
            }
        };
        let payload_len = payload.len();
        let provider_jwt = match self.provider_jwt(now) {
            Ok(jwt) => jwt,
            Err(error) => {
                return ProviderDelivery {
                    payload_len: 0,
                    outcome: ProviderOutcome::TransientError(format!(
                        "APNs provider JWT signing failed: {error}"
                    )),
                };
            }
        };
        let outcome = self
            .sender
            .send(ApnsRequest {
                environment: *environment,
                device_token: token.clone(),
                topic: self.topic.clone(),
                collapse_id: message.collapse_key,
                provider_jwt,
                payload,
            })
            .await;
        let outcome = match outcome {
            ApnsOutcome::Delivered { apns_id } => ProviderOutcome::Delivered {
                provider_id: apns_id,
            },
            ApnsOutcome::BadDeviceToken => ProviderOutcome::InvalidToken {
                reason: "BadDeviceToken".into(),
            },
            ApnsOutcome::Unregistered { timestamp_ms } => ProviderOutcome::InvalidToken {
                reason: format!("Unregistered at {timestamp_ms}"),
            },
            ApnsOutcome::TransientError(error) => ProviderOutcome::TransientError(error),
        };
        ProviderDelivery {
            payload_len,
            outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_P8: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPFauT/kbqwIxcoQW
BNxFLAfYXAa3OFmTIx3IcGqjUkyhRANCAATGtaYrLt8AL8cs25DIa+OeV4PCpUHt
SYW9s/UKX8shed4rIxRqMe3POJIY7OsF06EEtnyLrMjJg53H5HWAe2Mh
-----END PRIVATE KEY-----"#;

    struct MockApns {
        requests: Mutex<Vec<ApnsRequest>>,
        outcome: ApnsOutcome,
    }

    #[async_trait]
    impl ApnsSender for MockApns {
        async fn send(&self, request: ApnsRequest) -> ApnsOutcome {
            self.requests.lock().push(request);
            self.outcome.clone()
        }
    }

    fn provider(sender: Arc<MockApns>) -> ApnsProvider {
        let signer = ApnsProviderToken::new("KID", "TEAM", TEST_P8.as_bytes()).unwrap();
        ApnsProvider::new(sender, Arc::new(signer), "com.baybo.app")
    }

    fn target() -> PushTarget {
        PushTarget::Apns {
            token: "apns-token".into(),
            environment: ApnsEnvironment::Sandbox,
        }
    }

    fn message() -> EncryptedPush {
        EncryptedPush {
            collapse_key: "collapse".into(),
            bid: "device-1".into(),
            enc: "ciphertext".into(),
            n: "nonce".into(),
        }
    }

    #[test]
    fn environment_hosts_are_distinct() {
        assert_eq!(host(ApnsEnvironment::Sandbox), "api.sandbox.push.apple.com");
        assert_eq!(host(ApnsEnvironment::Production), "api.push.apple.com");
    }

    #[tokio::test]
    async fn adapter_builds_the_apns_payload_and_maps_delivery() {
        let sender = Arc::new(MockApns {
            requests: Mutex::new(Vec::new()),
            outcome: ApnsOutcome::Delivered {
                apns_id: Some("id-1".into()),
            },
        });
        let delivery = provider(Arc::clone(&sender))
            .send(&target(), message(), 1000)
            .await;
        assert_eq!(
            delivery.outcome,
            ProviderOutcome::Delivered {
                provider_id: Some("id-1".into())
            }
        );
        let requests = sender.requests.lock();
        let request = &requests[0];
        assert_eq!(request.environment, ApnsEnvironment::Sandbox);
        assert_eq!(request.device_token, "apns-token");
        assert_eq!(request.topic, "com.baybo.app");
        assert_eq!(request.collapse_id, "collapse");
        assert_eq!(request.payload.len(), delivery.payload_len);
        let payload: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
        assert_eq!(payload["aps"]["mutable-content"], 1);
        assert_eq!(payload["enc"], "ciphertext");
        assert_eq!(payload["n"], "nonce");
        assert_eq!(payload["bid"], "device-1");
    }

    #[tokio::test]
    async fn adapter_caches_then_refreshes_the_provider_jwt() {
        let sender = Arc::new(MockApns {
            requests: Mutex::new(Vec::new()),
            outcome: ApnsOutcome::Delivered { apns_id: None },
        });
        let provider = provider(Arc::clone(&sender));
        provider.send(&target(), message(), 1000).await;
        provider.send(&target(), message(), 1060).await;
        provider
            .send(
                &target(),
                message(),
                1000 + crate::jwt::TOKEN_REFRESH_SECS + 1,
            )
            .await;

        let requests = sender.requests.lock();
        assert_eq!(requests[0].provider_jwt, requests[1].provider_jwt);
        assert_ne!(requests[0].provider_jwt, requests[2].provider_jwt);
    }

    #[tokio::test]
    async fn adapter_maps_permanent_token_rejection() {
        let sender = Arc::new(MockApns {
            requests: Mutex::new(Vec::new()),
            outcome: ApnsOutcome::BadDeviceToken,
        });
        let delivery = provider(sender).send(&target(), message(), 1000).await;
        assert_eq!(
            delivery.outcome,
            ProviderOutcome::InvalidToken {
                reason: "BadDeviceToken".into()
            }
        );
    }
}
