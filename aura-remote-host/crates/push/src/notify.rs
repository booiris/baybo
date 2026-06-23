//! The `/notify` pipeline: admit → resolve token → sign → send blind → prune.
//!
//! A gateway (A) POSTs a [`NotifyRequest`] on every pushable turn. The push
//! role validates the instance key, looks up the device's APNs token, builds a
//! blind payload (a generic visible alert the NSE later rewrites, plus the
//! verbatim `enc`/`n`/`kid`/`bid`), signs the provider token, and sends via the
//! [`ApnsSender`] seam — pruning the token on a `400`/`410`. It never decrypts
//! `enc`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::apns::{ApnsOutcome, ApnsRequest, ApnsSender};
use crate::jwt::ApnsProviderToken;
use crate::store::{Admission, DeviceTokenStore};

/// The body a gateway POSTs to `/notify`. `enc`/`n` are the opaque base64 AEAD
/// ciphertext+tag and nonce, already encrypted by A with the device's push key
/// — copied verbatim, never decrypted here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub instance_key: String,
    pub device_id: String,
    pub collapse_id: String,
    pub kid: u32,
    /// Per-binding device id; the NSE selects the push key by this.
    pub bid: String,
    pub enc: String,
    pub n: String,
}

/// Result of processing one notify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    Delivered,
    /// `instance_key` is not admitted.
    Unadmitted,
    /// No APNs binding for `device_id`.
    UnknownDevice,
    /// APNs rejected the token (`400`/`410`) → the binding was pruned.
    Pruned,
    /// Transient transport / signing failure (retryable).
    Failed(String),
}

/// Wires admission + the device-token store + the provider-token signer + the
/// APNs seam into the `/notify` pipeline.
pub struct NotifyService {
    admission: Arc<dyn Admission>,
    store: Arc<dyn DeviceTokenStore>,
    sender: Arc<dyn ApnsSender>,
    signer: Arc<ApnsProviderToken>,
    /// Published app bundle id (`apns-topic`).
    topic: String,
}

impl NotifyService {
    pub fn new(
        admission: Arc<dyn Admission>,
        store: Arc<dyn DeviceTokenStore>,
        sender: Arc<dyn ApnsSender>,
        signer: Arc<ApnsProviderToken>,
        topic: impl Into<String>,
    ) -> Self {
        Self {
            admission,
            store,
            sender,
            signer,
            topic: topic.into(),
        }
    }

    /// Process one notify. `now` (unix seconds) stamps the provider token.
    pub async fn notify(&self, req: NotifyRequest, now: u64) -> NotifyOutcome {
        if !self.admission.is_admitted(&req.instance_key) {
            return NotifyOutcome::Unadmitted;
        }
        let Some(reg) = self.store.get(&req.device_id) else {
            return NotifyOutcome::UnknownDevice;
        };
        let jwt = match self.signer.sign(now) {
            Ok(j) => j,
            Err(e) => return NotifyOutcome::Failed(e.to_string()),
        };
        let apns_req = ApnsRequest {
            env: reg.env,
            device_token: reg.apns_token,
            topic: self.topic.clone(),
            collapse_id: req.collapse_id.clone(),
            provider_jwt: jwt,
            payload: build_payload(&req),
        };
        match self.sender.send(apns_req).await {
            ApnsOutcome::Delivered => NotifyOutcome::Delivered,
            ApnsOutcome::BadDeviceToken | ApnsOutcome::Unregistered { .. } => {
                // Unbind the APNs token only — never the gateway's device row.
                self.store.unbind(&req.device_id);
                NotifyOutcome::Pruned
            }
            ApnsOutcome::TransientError(e) => NotifyOutcome::Failed(e),
        }
    }
}

/// Build the blind APNs JSON body: a generic visible alert (`mutable-content:
/// 1` so the NSE fires and rewrites `title`/`body` after decrypting), plus the
/// operator-encrypted preview keys copied verbatim.
fn build_payload(req: &NotifyRequest) -> Vec<u8> {
    let body = json!({
        "aps": {
            "alert": { "title": "Aura", "body": "New message" },
            "mutable-content": 1,
        },
        "enc": req.enc,
        "n": req.n,
        "kid": req.kid,
        "bid": req.bid,
    });
    // `json!` always serializes; the impossible error degrades to an empty body
    // rather than panicking.
    serde_json::to_vec(&body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apns::ApnsEnv;
    use crate::store::{DeviceRegistration, InMemoryAdmission, InMemoryDeviceTokenStore};
    use async_trait::async_trait;
    use parking_lot::Mutex;

    const TEST_P8: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPFauT/kbqwIxcoQW
BNxFLAfYXAa3OFmTIx3IcGqjUkyhRANCAATGtaYrLt8AL8cs25DIa+OeV4PCpUHt
SYW9s/UKX8shed4rIxRqMe3POJIY7OsF06EEtnyLrMjJg53H5HWAe2Mh
-----END PRIVATE KEY-----"#;

    struct MockApns {
        outcome: ApnsOutcome,
        last: Mutex<Option<ApnsRequest>>,
    }

    impl MockApns {
        fn new(outcome: ApnsOutcome) -> Self {
            Self {
                outcome,
                last: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl ApnsSender for MockApns {
        async fn send(&self, req: ApnsRequest) -> ApnsOutcome {
            *self.last.lock() = Some(req);
            self.outcome.clone()
        }
    }

    fn signer() -> Arc<ApnsProviderToken> {
        Arc::new(ApnsProviderToken::new("KID", "TEAM", TEST_P8.as_bytes()).unwrap())
    }

    fn req(instance: &str, device: &str) -> NotifyRequest {
        NotifyRequest {
            instance_key: instance.into(),
            device_id: device.into(),
            collapse_id: "dev-1:sess-1".into(),
            kid: 0,
            bid: "dev-1".into(),
            enc: "Y2lwaGVydGV4dA==".into(),
            n: "bm9uY2U=".into(),
        }
    }

    fn service(sender: Arc<MockApns>, store: Arc<InMemoryDeviceTokenStore>) -> NotifyService {
        NotifyService::new(
            Arc::new(InMemoryAdmission::with_keys(["inst-A"])),
            store,
            sender,
            signer(),
            "com.aura.app",
        )
    }

    #[tokio::test]
    async fn unadmitted_instance_key_rejected() {
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let svc = service(Arc::new(MockApns::new(ApnsOutcome::Delivered)), store);
        assert_eq!(
            svc.notify(req("bad-instance", "dev-1"), 1000).await,
            NotifyOutcome::Unadmitted,
        );
    }

    #[tokio::test]
    async fn unknown_device_rejected() {
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let svc = service(Arc::new(MockApns::new(ApnsOutcome::Delivered)), store);
        assert_eq!(
            svc.notify(req("inst-A", "ghost"), 1000).await,
            NotifyOutcome::UnknownDevice,
        );
    }

    #[tokio::test]
    async fn happy_path_builds_blind_payload_and_delivers() {
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        store.register(
            "dev-1",
            DeviceRegistration {
                apns_token: "apns-tok-xyz".into(),
                env: ApnsEnv::Sandbox,
            },
        );
        let sender = Arc::new(MockApns::new(ApnsOutcome::Delivered));
        let svc = service(Arc::clone(&sender), store);
        assert_eq!(
            svc.notify(req("inst-A", "dev-1"), 1000).await,
            NotifyOutcome::Delivered,
        );

        let sent = sender.last.lock().clone().expect("apns request sent");
        assert_eq!(sent.env, ApnsEnv::Sandbox);
        assert_eq!(sent.device_token, "apns-tok-xyz");
        assert_eq!(sent.topic, "com.aura.app");
        assert_eq!(sent.collapse_id, "dev-1:sess-1");
        assert!(!sent.provider_jwt.is_empty());

        // Payload is the verbatim encrypted preview + a mutable-content alert.
        let v: serde_json::Value = serde_json::from_slice(&sent.payload).unwrap();
        assert_eq!(v["aps"]["mutable-content"], 1);
        assert_eq!(v["enc"], "Y2lwaGVydGV4dA==");
        assert_eq!(v["n"], "bm9uY2U=");
        assert_eq!(v["kid"], 0);
        assert_eq!(v["bid"], "dev-1");
    }

    #[tokio::test]
    async fn bad_token_prunes_binding() {
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        store.register(
            "dev-1",
            DeviceRegistration {
                apns_token: "dead".into(),
                env: ApnsEnv::Production,
            },
        );
        let svc = service(Arc::new(MockApns::new(ApnsOutcome::BadDeviceToken)), Arc::clone(&store));
        assert_eq!(
            svc.notify(req("inst-A", "dev-1"), 1000).await,
            NotifyOutcome::Pruned,
        );
        assert!(store.get("dev-1").is_none(), "dead token unbound");
    }

    #[tokio::test]
    async fn unregistered_410_prunes_binding() {
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        store.register(
            "dev-1",
            DeviceRegistration {
                apns_token: "gone".into(),
                env: ApnsEnv::Production,
            },
        );
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::Unregistered {
                timestamp_ms: 42,
            })),
            Arc::clone(&store),
        );
        assert_eq!(
            svc.notify(req("inst-A", "dev-1"), 1000).await,
            NotifyOutcome::Pruned,
        );
        assert!(store.get("dev-1").is_none());
    }

    #[tokio::test]
    async fn transient_error_keeps_binding() {
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        store.register(
            "dev-1",
            DeviceRegistration {
                apns_token: "live".into(),
                env: ApnsEnv::Sandbox,
            },
        );
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::TransientError("503".into()))),
            Arc::clone(&store),
        );
        assert_eq!(
            svc.notify(req("inst-A", "dev-1"), 1000).await,
            NotifyOutcome::Failed("503".into()),
        );
        assert!(store.get("dev-1").is_some(), "transient error must not prune");
    }
}
