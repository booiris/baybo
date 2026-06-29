//! The `/notify` pipeline: resolve token → verify → sign → send blind → prune.
//!
//! A gateway (A) POSTs a [`NotifyRequest`] on every pushable turn. The push role
//! looks up the device's APNs token, verifies the notify under the gateway push
//! key the device delegated at `/register`, builds a blind payload (a generic
//! visible alert the NSE later rewrites, plus the verbatim `enc`/`n`/`bid`), signs
//! the provider token, and sends via the [`ApnsSender`] seam — pruning the token
//! on a `400`/`410`. It never decrypts `enc`. The routes are keyless: the
//! delegation chain, not a caller key, authorizes every binding and notify.

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use crate::apns::{ApnsEnv, ApnsOutcome, ApnsRequest, ApnsSender};
use crate::delegation;
use crate::error::PushError;
use crate::jwt::ApnsProviderToken;
use crate::ratelimit::NotifyRateLimiter;
use crate::store::{DeviceRegistration, DeviceTokenStore};

/// The `/notify` + `/register` request bodies live in the shared protocol crate,
/// so the gateway POSTs the exact same shapes (re-exported here for the rest of
/// the push crate).
pub use remote_host_protocol::push::{NotifyRequest, RegisterRequest};

/// Result of processing one `/register`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered,
    /// The `apns_token` is implausibly long, so it is refused before an oversized
    /// junk token can bloat a stored binding (`400`).
    InvalidToken,
    /// The device→gateway delegation, the register signature, or the replay
    /// counter failed verification — the caller could not prove ownership of the
    /// `device_id`, so the binding is left untouched. The delegation chain is what
    /// stops anyone from hijacking another device's binding.
    Rejected,
    /// The device-token store is at its hard cap with no idle/unconfirmed binding
    /// to evict, so the registration is shed (`503`) rather than growing the store
    /// without bound.
    Capacity,
}

/// Result of processing one notify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    Delivered,
    /// `device_id` is pushing faster than its allowed rate; the gateway should
    /// back off and retry (`429`).
    RateLimited,
    /// No APNs binding for `device_id`.
    UnknownDevice,
    /// The notify signature (under the gateway key the device delegated) or its
    /// replay counter failed — no one can push to a device it doesn't own.
    Rejected,
    /// APNs rejected the token (`400`/`410`) → the binding was pruned.
    Pruned,
    /// Transient transport / signing failure (retryable).
    Failed(String),
}

/// Wires the device-token store + the provider-token signer + the APNs seam into
/// the `/notify` pipeline.
pub struct NotifyService {
    store: Arc<dyn DeviceTokenStore>,
    sender: Arc<dyn ApnsSender>,
    signer: Arc<ApnsProviderToken>,
    /// Per-`device_id` frequency control, checked before any APNs work so a flood
    /// is cheap to refuse.
    rate: NotifyRateLimiter,
    /// The last signed provider JWT and its `iat`, reused across requests until
    /// it ages past the refresh window (APNs accepts a token for up to an hour).
    cached_jwt: Mutex<Option<(String, u64)>>,
    /// Published app bundle id (`apns-topic`).
    topic: String,
}

impl NotifyService {
    pub fn new(
        store: Arc<dyn DeviceTokenStore>,
        sender: Arc<dyn ApnsSender>,
        signer: Arc<ApnsProviderToken>,
        topic: impl Into<String>,
        rate: NotifyRateLimiter,
    ) -> Self {
        Self {
            store,
            sender,
            signer,
            rate,
            cached_jwt: Mutex::new(None),
            topic: topic.into(),
        }
    }

    /// A provider JWT valid at `now`, re-signing (and caching) only when the
    /// cached one is missing or older than [`crate::jwt::TOKEN_REFRESH_SECS`].
    /// APNs accepts a token for ~an hour, so this signs roughly once per refresh
    /// window instead of an ES256 signature per push.
    fn provider_jwt(&self, now: u64) -> Result<String, PushError> {
        if let Some((jwt, issued_at)) = self.cached_jwt.lock().as_ref()
            && !ApnsProviderToken::needs_refresh(*issued_at, now)
        {
            return Ok(jwt.clone());
        }
        let jwt = self.signer.sign(now)?;
        *self.cached_jwt.lock() = Some((jwt.clone(), now));
        Ok(jwt)
    }

    /// Bind (or rebind) a device's APNs token, authorized by the device→gateway
    /// delegation chain. The push role's only per-device write outside `/notify`.
    pub fn register(&self, req: RegisterRequest) -> RegisterOutcome {
        // Cap the `apns_token` length cheaply, before any Ed25519 work — a
        // self-signed chain can sign any string, so bound how much one stored
        // binding can hold. A bogus-but-bounded token is caught later at /notify by
        // APNs (`BadDeviceToken` → prune).
        if req.apns_token.len() > APNS_TOKEN_MAX_LEN {
            return RegisterOutcome::InvalidToken;
        }
        // Verify the device→gateway delegation and the gateway's signature over
        // the binding. `device_id` self-certifies the device key, so C needs no
        // stored secret and no trust-on-first-use; without the device's key no one
        // can forge either signature.
        let Some(device_pub) = delegation::device_pubkey_from_id(&req.device_id) else {
            return RegisterOutcome::Rejected;
        };
        let (Some(gateway_pub), Some(deleg_sig), Some(reg_sig)) = (
            b64_decode(&req.gateway_pubkey).and_then(|b| delegation::verifying_key_from_bytes(&b)),
            b64_decode(&req.delegation).and_then(|b| delegation::signature_from_bytes(&b)),
            b64_decode(&req.sig).and_then(|b| delegation::signature_from_bytes(&b)),
        ) else {
            return RegisterOutcome::Rejected;
        };
        if !delegation::verify_delegation(&device_pub, &gateway_pub, &deleg_sig) {
            return RegisterOutcome::Rejected;
        }
        if !delegation::verify_register(
            &gateway_pub,
            &req.device_id,
            &req.apns_token,
            env_byte(req.env),
            req.counter,
            &reg_sig,
        ) {
            return RegisterOutcome::Rejected;
        }
        // Replay floor: a re-register must strictly advance the counter.
        if let Some(existing) = self.store.get(&req.device_id)
            && req.counter <= existing.last_counter
        {
            return RegisterOutcome::Rejected;
        }
        if !self.store.register(
            &req.device_id,
            DeviceRegistration {
                apns_token: req.apns_token,
                env: req.env,
                gateway_pubkey: gateway_pub.to_bytes(),
                last_counter: req.counter,
            },
        ) {
            return RegisterOutcome::Capacity;
        }
        RegisterOutcome::Registered
    }

    /// Process one notify. `now` (unix seconds) stamps the provider token.
    pub async fn notify(&self, req: NotifyRequest, now: u64) -> NotifyOutcome {
        // Resolve the device BEFORE any heavier work (and before minting a rate
        // bucket): an unknown `device_id` is refused here, so the bucket map is
        // bounded by the registered set, not by attacker-supplied ids.
        let Some(reg) = self.store.get(&req.device_id) else {
            return NotifyOutcome::UnknownDevice;
        };
        // Authenticate the notify under the gateway key the device delegated
        // (stored at register) — without that key no one can spam or redirect this
        // device, even knowing the `device_id`.
        let Some(gateway_pub) = delegation::verifying_key_from_bytes(&reg.gateway_pubkey) else {
            return NotifyOutcome::Rejected;
        };
        let Some(sig) = b64_decode(&req.sig).and_then(|b| delegation::signature_from_bytes(&b))
        else {
            return NotifyOutcome::Rejected;
        };
        if !delegation::verify_notify(
            &gateway_pub,
            &req.device_id,
            &req.enc,
            &req.n,
            &req.bid,
            req.counter,
            &sig,
        ) {
            return NotifyOutcome::Rejected;
        }
        // Replay floor (atomic check-and-advance): reject a counter that doesn't
        // strictly exceed the last accepted one.
        if !self.store.advance_counter(&req.device_id, req.counter) {
            return NotifyOutcome::Rejected;
        }
        // Per-device frequency control gates before signing / egress.
        if !self.rate.check(&req.device_id) {
            return NotifyOutcome::RateLimited;
        }
        let jwt = match self.provider_jwt(now) {
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
            ApnsOutcome::Delivered => {
                // APNs accepted the token → a real device is behind this binding.
                // Confirm it so it is exempt from idle eviction and refresh its
                // idle clock; a register-only flood is never confirmed and ages out.
                self.store.confirm(&req.device_id);
                NotifyOutcome::Delivered
            }
            ApnsOutcome::BadDeviceToken | ApnsOutcome::Unregistered { .. } => {
                // Unbind the APNs token only — never the gateway's device row.
                self.store.unbind(&req.device_id);
                NotifyOutcome::Pruned
            }
            ApnsOutcome::TransientError(e) => NotifyOutcome::Failed(e),
        }
    }
}

/// Max accepted `apns_token` length, in chars. Real APNs device tokens are hex of
/// ~32–100 bytes (64–200 chars); this generous cap just bounds the memory one
/// stored binding can occupy (and rejects an oversized junk token) without
/// rejecting a future longer token. Token *validity* isn't checked here — a bogus
/// token is caught at `/notify` by APNs `BadDeviceToken` → prune.
const APNS_TOKEN_MAX_LEN: usize = 256;

/// Decode a base64-standard wire field; `None` on malformed input (→ reject).
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Canonical `env` byte for the signed register message (matches `device-proto`).
fn env_byte(env: ApnsEnv) -> u8 {
    match env {
        ApnsEnv::Sandbox => delegation::ENV_SANDBOX,
        ApnsEnv::Production => delegation::ENV_PRODUCTION,
    }
}

/// Build the blind APNs JSON body: a generic visible alert (`mutable-content:
/// 1` so the NSE fires and rewrites `title`/`body` after decrypting), plus the
/// operator-encrypted preview keys copied verbatim.
fn build_payload(req: &NotifyRequest) -> Vec<u8> {
    let body = json!({
        "aps": {
            "alert": { "title": "Baybo", "body": "New message" },
            "mutable-content": 1,
        },
        "enc": req.enc,
        "n": req.n,
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
    use crate::delegation::{ENV_SANDBOX, test_sign};
    use crate::store::{DeviceRegistration, InMemoryDeviceTokenStore};
    use async_trait::async_trait;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
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

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// A plausible APNs device token (32 bytes hex) that passes the `/register`
    /// shape check — the chain/replay tests care about the chain, not the token,
    /// but the token must clear the length cap to reach those code paths.
    const APNS_TOK: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// The device identity (seed 1) and its delegated gateway push key (seed 2),
    /// plus the `device_id` derived from the device key.
    fn keys() -> (SigningKey, SigningKey, String) {
        let device = test_sign::signing_key(1);
        let gateway = test_sign::signing_key(2);
        let device_id = test_sign::device_id_for(&device.verifying_key());
        (device, gateway, device_id)
    }

    /// Pre-seed the store with a binding owned by `gateway` (as a verified
    /// `/register` would leave it), so notify tests can exercise the notify path.
    fn seed(store: &InMemoryDeviceTokenStore, gateway: &SigningKey, device_id: &str, token: &str) {
        store.register(
            device_id,
            DeviceRegistration {
                apns_token: token.into(),
                env: ApnsEnv::Sandbox,
                gateway_pubkey: gateway.verifying_key().to_bytes(),
                last_counter: 0,
            },
        );
    }

    /// A `/notify` signed by `gateway` for `device_id` at `counter`.
    fn notify_req(gateway: &SigningKey, device_id: &str, counter: u64) -> NotifyRequest {
        let (enc, n) = ("Y2lwaGVydGV4dA==", "bm9uY2U=");
        let sig = test_sign::sign_notify(gateway, device_id, enc, n, device_id, counter);
        NotifyRequest {
            device_id: device_id.into(),
            collapse_id: "collapse".into(),
            bid: device_id.into(),
            enc: enc.into(),
            n: n.into(),
            sig: b64(&sig.to_bytes()),
            counter,
        }
    }

    /// A `/register` with `device` delegating to `gateway` (the device_id must be
    /// `device`'s own for the chain to verify).
    fn register_req(
        device: &SigningKey,
        gateway: &SigningKey,
        device_id: &str,
        token: &str,
        counter: u64,
    ) -> RegisterRequest {
        let deleg = test_sign::sign_delegation(device, &gateway.verifying_key());
        let sig = test_sign::sign_register(gateway, device_id, token, ENV_SANDBOX, counter);
        RegisterRequest {
            device_id: device_id.into(),
            apns_token: token.into(),
            env: ApnsEnv::Sandbox,
            gateway_pubkey: b64(&gateway.verifying_key().to_bytes()),
            delegation: b64(&deleg.to_bytes()),
            sig: b64(&sig.to_bytes()),
            counter,
        }
    }

    fn service(sender: Arc<MockApns>, store: Arc<InMemoryDeviceTokenStore>) -> NotifyService {
        NotifyService::new(
            store,
            sender,
            signer(),
            "com.baybo.app",
            NotifyRateLimiter::default(),
        )
    }

    #[test]
    fn register_binds_token_when_chain_verifies() {
        let (device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::Delivered)),
            Arc::clone(&store),
        );

        assert_eq!(
            svc.register(register_req(&device, &gateway, &device_id, APNS_TOK, 1)),
            RegisterOutcome::Registered,
        );
        let stored = store.get(&device_id).unwrap();
        assert_eq!(stored.apns_token, APNS_TOK);
        assert_eq!(stored.gateway_pubkey, gateway.verifying_key().to_bytes());
        assert_eq!(stored.last_counter, 1);
    }

    #[test]
    fn register_rejects_a_device_the_caller_does_not_own() {
        // device_id belongs to `device` (seed 1), but the delegation is signed by
        // an impostor key — so the chain doesn't verify and the binding is refused.
        let (device, gateway, device_id) = keys();
        let impostor = test_sign::signing_key(9);
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::Delivered)),
            store.clone(),
        );
        assert_eq!(
            svc.register(register_req(&impostor, &gateway, &device_id, APNS_TOK, 1)),
            RegisterOutcome::Rejected,
        );
        assert!(store.get(&device_id).is_none());
        // Sanity: the real owner's delegation does bind.
        assert_eq!(
            svc.register(register_req(&device, &gateway, &device_id, APNS_TOK, 1)),
            RegisterOutcome::Registered,
        );
    }

    #[test]
    fn register_rejects_replayed_counter() {
        let (device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let svc = service(Arc::new(MockApns::new(ApnsOutcome::Delivered)), store);
        assert_eq!(
            svc.register(register_req(&device, &gateway, &device_id, APNS_TOK, 5)),
            RegisterOutcome::Registered,
        );
        // Same/older counter is a replay.
        assert_eq!(
            svc.register(register_req(&device, &gateway, &device_id, APNS_TOK, 5)),
            RegisterOutcome::Rejected,
        );
    }

    #[test]
    fn register_rejects_an_oversized_apns_token() {
        // An implausibly long token is refused before it can bloat a stored
        // binding, even though the (self-signed) chain would otherwise verify.
        let (device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::Delivered)),
            Arc::clone(&store),
        );
        let oversized = "a".repeat(APNS_TOKEN_MAX_LEN + 1);
        assert_eq!(
            svc.register(register_req(&device, &gateway, &device_id, &oversized, 1)),
            RegisterOutcome::InvalidToken,
        );
        assert!(
            store.get(&device_id).is_none(),
            "oversized token never stored"
        );
        // A normal-length token binds.
        assert_eq!(
            svc.register(register_req(&device, &gateway, &device_id, APNS_TOK, 1)),
            RegisterOutcome::Registered,
        );
    }

    #[test]
    fn register_sheds_when_store_is_full_with_nothing_evictable() {
        // A cap-1 store bound to a fresh unconfirmed device refuses a second
        // distinct device with `Capacity` rather than growing — bounded under a
        // sustained register flood.
        let store = Arc::new(InMemoryDeviceTokenStore::with_limits(
            1,
            std::time::Duration::from_secs(60),
        ));
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::Delivered)),
            Arc::clone(&store),
        );
        let (device, gateway, device_id) = keys();
        assert_eq!(
            svc.register(register_req(&device, &gateway, &device_id, APNS_TOK, 1)),
            RegisterOutcome::Registered,
        );
        // A second device (distinct identity, valid self-signed chain) finds the
        // store full of a fresh unconfirmed binding → shed with Capacity.
        let device2 = test_sign::signing_key(11);
        let gateway2 = test_sign::signing_key(12);
        let device_id2 = test_sign::device_id_for(&device2.verifying_key());
        assert_eq!(
            svc.register(register_req(&device2, &gateway2, &device_id2, APNS_TOK, 1)),
            RegisterOutcome::Capacity,
        );
        assert_eq!(store.len(), 1, "store stays bounded at the cap");
    }

    #[tokio::test]
    async fn over_rate_returns_rate_limited_before_touching_apns() {
        let (_d, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id, "tok");
        let svc = service(Arc::new(MockApns::new(ApnsOutcome::Delivered)), store);
        // The whole burst delivers (each with a fresh, increasing counter); the
        // next push over the burst is rate-limited.
        for i in 1..=crate::ratelimit::NOTIFY_BURST as u64 {
            assert_eq!(
                svc.notify(notify_req(&gateway, &device_id, i), 1000).await,
                NotifyOutcome::Delivered,
            );
        }
        assert_eq!(
            svc.notify(
                notify_req(
                    &gateway,
                    &device_id,
                    crate::ratelimit::NOTIFY_BURST as u64 + 1
                ),
                1000
            )
            .await,
            NotifyOutcome::RateLimited,
        );
    }

    #[tokio::test]
    async fn unknown_device_rejected() {
        let (_d, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let svc = service(Arc::new(MockApns::new(ApnsOutcome::Delivered)), store);
        assert_eq!(
            svc.notify(notify_req(&gateway, &device_id, 1), 1000).await,
            NotifyOutcome::UnknownDevice,
        );
    }

    #[tokio::test]
    async fn notify_rejects_a_foreign_gateway_key_and_a_replay() {
        // The binding is owned by `gateway` (the device delegated to it). A notify
        // signed with a *different* gateway key can't push it; nor can a replay of
        // an already-accepted counter.
        let (_d, gateway, device_id) = keys();
        let impostor_gateway = test_sign::signing_key(9);
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id, "tok");
        let svc = service(Arc::new(MockApns::new(ApnsOutcome::Delivered)), store);
        // Signed with a gateway key the device never delegated → rejected.
        assert_eq!(
            svc.notify(notify_req(&impostor_gateway, &device_id, 1), 1000)
                .await,
            NotifyOutcome::Rejected,
        );
        // The legitimate gateway delivers.
        assert_eq!(
            svc.notify(notify_req(&gateway, &device_id, 1), 1000).await,
            NotifyOutcome::Delivered,
        );
        // Replaying that same counter is rejected.
        assert_eq!(
            svc.notify(notify_req(&gateway, &device_id, 1), 1000).await,
            NotifyOutcome::Rejected,
        );
    }

    #[tokio::test]
    async fn happy_path_builds_blind_payload_and_delivers() {
        let (_d, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id, "apns-tok-xyz");
        let sender = Arc::new(MockApns::new(ApnsOutcome::Delivered));
        let svc = service(Arc::clone(&sender), store);
        assert_eq!(
            svc.notify(notify_req(&gateway, &device_id, 1), 1000).await,
            NotifyOutcome::Delivered,
        );

        let sent = sender.last.lock().clone().expect("apns request sent");
        assert_eq!(sent.env, ApnsEnv::Sandbox);
        assert_eq!(sent.device_token, "apns-tok-xyz");
        assert_eq!(sent.topic, "com.baybo.app");
        assert_eq!(sent.collapse_id, "collapse");
        assert!(!sent.provider_jwt.is_empty());

        // Payload is the verbatim encrypted preview + a mutable-content alert.
        let v: serde_json::Value = serde_json::from_slice(&sent.payload).unwrap();
        assert_eq!(v["aps"]["mutable-content"], 1);
        assert_eq!(v["enc"], "Y2lwaGVydGV4dA==");
        assert_eq!(v["n"], "bm9uY2U=");
        assert_eq!(v["bid"], device_id);
    }

    #[tokio::test]
    async fn bad_token_prunes_binding() {
        let (_d, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id, "dead");
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::BadDeviceToken)),
            Arc::clone(&store),
        );
        assert_eq!(
            svc.notify(notify_req(&gateway, &device_id, 1), 1000).await,
            NotifyOutcome::Pruned,
        );
        assert!(store.get(&device_id).is_none(), "dead token unbound");
    }

    #[tokio::test]
    async fn unregistered_410_prunes_binding() {
        let (_d, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id, "gone");
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::Unregistered {
                timestamp_ms: 42,
            })),
            Arc::clone(&store),
        );
        assert_eq!(
            svc.notify(notify_req(&gateway, &device_id, 1), 1000).await,
            NotifyOutcome::Pruned,
        );
        assert!(store.get(&device_id).is_none());
    }

    #[tokio::test]
    async fn transient_error_keeps_binding() {
        let (_d, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id, "live");
        let svc = service(
            Arc::new(MockApns::new(ApnsOutcome::TransientError("503".into()))),
            Arc::clone(&store),
        );
        assert_eq!(
            svc.notify(notify_req(&gateway, &device_id, 1), 1000).await,
            NotifyOutcome::Failed("503".into()),
        );
        assert!(
            store.get(&device_id).is_some(),
            "transient error must not prune"
        );
    }

    #[tokio::test]
    async fn provider_token_cached_within_window_and_refreshed_after() {
        let (_d, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id, "t");
        let sender = Arc::new(MockApns::new(ApnsOutcome::Delivered));
        let svc = service(Arc::clone(&sender), store);

        svc.notify(notify_req(&gateway, &device_id, 1), 1000).await;
        let jwt1 = sender.last.lock().clone().unwrap().provider_jwt;
        // A later push inside the refresh window reuses the same signed token.
        svc.notify(notify_req(&gateway, &device_id, 2), 1000 + 60)
            .await;
        let jwt2 = sender.last.lock().clone().unwrap().provider_jwt;
        assert_eq!(jwt1, jwt2, "token reused within the refresh window");
        // Past the window it is re-signed (new `iat`).
        svc.notify(
            notify_req(&gateway, &device_id, 3),
            1000 + crate::jwt::TOKEN_REFRESH_SECS + 1,
        )
        .await;
        let jwt3 = sender.last.lock().clone().unwrap().provider_jwt;
        assert_ne!(jwt1, jwt3, "token re-signed past the refresh window");
    }
}
