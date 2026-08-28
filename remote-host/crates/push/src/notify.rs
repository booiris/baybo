//! Provider-neutral `/register` and `/notify` pipeline.
//!
//! The service authenticates provider-tagged bindings and encrypted previews,
//! enforces replay/rate limits, then dispatches through [`PushProviders`]. It
//! never decrypts `enc`; APNs/FCM framing and credentials stay inside their
//! provider adapters.

use std::sync::Arc;

use crate::delegation;
use crate::provider::{EncryptedPush, ProviderOutcome, PushProviders};
use crate::ratelimit::NotifyRateLimiter;
use crate::store::{DeviceRegistration, DeviceTokenStore};
use crate::traffic::PushTrafficRegistry;
use remote_host_protocol::{device_id_log, push::PUSH_TOKEN_MAX_LEN};

pub use remote_host_protocol::push::{NotifyRequest, RegisterRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered,
    InvalidToken,
    ProviderUnavailable,
    Rejected,
    Capacity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    Delivered,
    RateLimited,
    UnknownDevice,
    ProviderUnavailable,
    Rejected,
    Pruned,
    Failed(String),
}

pub struct NotifyService {
    store: Arc<dyn DeviceTokenStore>,
    providers: Arc<PushProviders>,
    traffic: Arc<PushTrafficRegistry>,
    rate: NotifyRateLimiter,
}

impl NotifyService {
    pub fn new(
        store: Arc<dyn DeviceTokenStore>,
        providers: Arc<PushProviders>,
        traffic: Arc<PushTrafficRegistry>,
        rate: NotifyRateLimiter,
    ) -> Self {
        Self {
            store,
            providers,
            traffic,
            rate,
        }
    }

    pub fn register(&self, req: RegisterRequest) -> RegisterOutcome {
        let provider = req.target.provider();
        let token_len = req.target.token().len();
        if token_len == 0 || token_len > PUSH_TOKEN_MAX_LEN {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                provider = provider.as_str(),
                token_len,
                cap = PUSH_TOKEN_MAX_LEN,
                "push: register rejected — provider token has invalid length"
            );
            return RegisterOutcome::InvalidToken;
        }
        if !self.providers.supports(provider) {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                provider = provider.as_str(),
                "push: register rejected — provider is not configured"
            );
            return RegisterOutcome::ProviderUnavailable;
        }

        let Some(device_pub) = delegation::device_pubkey_from_id(&req.device_id) else {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                device_id_len = req.device_id.len(),
                "push: register rejected — device_id does not parse to a device pubkey"
            );
            return RegisterOutcome::Rejected;
        };
        let gateway_pub = b64_decode(&req.gateway_pubkey)
            .and_then(|bytes| delegation::verifying_key_from_bytes(&bytes));
        let delegation_signature =
            b64_decode(&req.delegation).and_then(|bytes| delegation::signature_from_bytes(&bytes));
        let register_signature =
            b64_decode(&req.sig).and_then(|bytes| delegation::signature_from_bytes(&bytes));
        let (Some(gateway_pub), Some(delegation_signature), Some(register_signature)) =
            (gateway_pub, delegation_signature, register_signature)
        else {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                "push: register rejected — malformed key/signature wire field"
            );
            return RegisterOutcome::Rejected;
        };
        if !delegation::verify_delegation(&device_pub, &gateway_pub, &delegation_signature) {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                gateway_pubkey_prefix = %pubkey_prefix(&gateway_pub.to_bytes()),
                "push: register rejected — device delegation verify failed"
            );
            return RegisterOutcome::Rejected;
        }
        if !delegation::verify_register(
            &gateway_pub,
            &req.device_id,
            &req.target,
            req.counter,
            &register_signature,
        ) {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                provider = provider.as_str(),
                counter = req.counter,
                "push: register rejected — request signature verify failed"
            );
            return RegisterOutcome::Rejected;
        }
        if let Some(existing) = self.store.get(&req.device_id)
            && req.counter <= existing.last_counter
        {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                got = req.counter,
                floor = existing.last_counter,
                "push: register rejected — replay counter not above stored floor"
            );
            return RegisterOutcome::Rejected;
        }

        if !self.store.register(
            &req.device_id,
            DeviceRegistration {
                target: req.target,
                gateway_pubkey: gateway_pub.to_bytes(),
                last_counter: req.counter,
            },
        ) {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                store_len = self.store.len(),
                "push: register shed — device store at cap with nothing evictable"
            );
            return RegisterOutcome::Capacity;
        }
        tracing::info!(
            device_id = %device_id_log(&req.device_id),
            provider = provider.as_str(),
            token_len,
            counter = req.counter,
            "push: device binding registered"
        );
        RegisterOutcome::Registered
    }

    pub async fn notify(&self, req: NotifyRequest, now: u64) -> NotifyOutcome {
        let Some(registration) = self.store.get(&req.device_id) else {
            tracing::info!(
                device_id = %device_id_log(&req.device_id),
                store_len = self.store.len(),
                "push: notify for unknown device"
            );
            return NotifyOutcome::UnknownDevice;
        };
        let provider = registration.target.provider();
        let Some(gateway_pub) = delegation::verifying_key_from_bytes(&registration.gateway_pubkey)
        else {
            tracing::error!(
                device_id = %device_id_log(&req.device_id),
                stored_gateway_pubkey_prefix = %pubkey_prefix(&registration.gateway_pubkey),
                "push: stored gateway pubkey no longer parses"
            );
            return NotifyOutcome::Rejected;
        };
        let Some(signature) =
            b64_decode(&req.sig).and_then(|bytes| delegation::signature_from_bytes(&bytes))
        else {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                "push: notify rejected — signature field is malformed"
            );
            return NotifyOutcome::Rejected;
        };
        if !delegation::verify_notify(&gateway_pub, &req.signing_input(), &signature) {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                counter = req.counter,
                "push: notify rejected — request signature verify failed"
            );
            return NotifyOutcome::Rejected;
        }
        if !self.store.advance_counter(&req.device_id, req.counter) {
            tracing::warn!(
                device_id = %device_id_log(&req.device_id),
                got = req.counter,
                floor = registration.last_counter,
                "push: notify rejected — replay counter not above stored floor"
            );
            return NotifyOutcome::Rejected;
        }
        if !self.rate.check(&req.device_id) {
            tracing::debug!(
                device_id = %device_id_log(&req.device_id),
                "push: notify rate-limited for device"
            );
            return NotifyOutcome::RateLimited;
        }

        let message = EncryptedPush {
            collapse_key: req.collapse_key.clone(),
            bid: req.bid,
            enc: req.enc,
            n: req.n,
        };
        let Some(delivery) = self
            .providers
            .send(&registration.target, message, now)
            .await
        else {
            tracing::error!(
                device_id = %device_id_log(&req.device_id),
                provider = provider.as_str(),
                "push: registered provider is not configured"
            );
            return NotifyOutcome::ProviderUnavailable;
        };
        if delivery.payload_len > 0 {
            self.traffic.record(&req.device_id, delivery.payload_len);
        }

        match delivery.outcome {
            ProviderOutcome::Delivered { provider_id } => {
                self.store.confirm(&req.device_id);
                tracing::debug!(
                    device_id = %device_id_log(&req.device_id),
                    provider = provider.as_str(),
                    collapse_key = %req.collapse_key,
                    payload_len = delivery.payload_len,
                    provider_id = provider_id.as_deref().unwrap_or("<none>"),
                    "push: provider accepted notification"
                );
                NotifyOutcome::Delivered
            }
            ProviderOutcome::InvalidToken { reason } => {
                self.store.unbind(&req.device_id);
                tracing::info!(
                    device_id = %device_id_log(&req.device_id),
                    provider = provider.as_str(),
                    reason,
                    "push: provider rejected token; binding pruned"
                );
                NotifyOutcome::Pruned
            }
            ProviderOutcome::TransientError(error) => {
                tracing::warn!(
                    device_id = %device_id_log(&req.device_id),
                    provider = provider.as_str(),
                    error,
                    "push: provider send failed; binding kept"
                );
                NotifyOutcome::Failed(error)
            }
        }
    }
}

fn pubkey_prefix(key: &[u8; 32]) -> String {
    hex::encode(&key[..4])
}

fn b64_decode(value: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use parking_lot::Mutex;
    use remote_host_protocol::push::{ApnsEnvironment, PushProvider, PushTarget};

    use crate::delegation::test_sign;
    use crate::provider::{ProviderDelivery, ProviderSender};
    use crate::store::InMemoryDeviceTokenStore;

    const APNS_TOKEN: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    struct MockProvider {
        provider: PushProvider,
        outcome: ProviderOutcome,
        last: Mutex<Option<EncryptedPush>>,
    }

    impl MockProvider {
        fn new(provider: PushProvider, outcome: ProviderOutcome) -> Self {
            Self {
                provider,
                outcome,
                last: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl ProviderSender for MockProvider {
        fn provider(&self) -> PushProvider {
            self.provider
        }

        async fn send(
            &self,
            _target: &PushTarget,
            message: EncryptedPush,
            _now: u64,
        ) -> ProviderDelivery {
            *self.last.lock() = Some(message);
            ProviderDelivery {
                payload_len: 123,
                outcome: self.outcome.clone(),
            }
        }
    }

    fn keys() -> (SigningKey, SigningKey, String) {
        let device = test_sign::signing_key(1);
        let gateway = test_sign::signing_key(2);
        let device_id = test_sign::device_id_for(&device.verifying_key());
        (device, gateway, device_id)
    }

    fn apns_target(token: &str) -> PushTarget {
        PushTarget::Apns {
            token: token.into(),
            environment: ApnsEnvironment::Sandbox,
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn register_request(
        device: &SigningKey,
        gateway: &SigningKey,
        device_id: &str,
        target: PushTarget,
        counter: u64,
    ) -> RegisterRequest {
        let delegation = test_sign::sign_delegation(device, &gateway.verifying_key());
        let signature = test_sign::sign_register(gateway, device_id, &target, counter);
        RegisterRequest {
            device_id: device_id.into(),
            target,
            gateway_pubkey: b64(&gateway.verifying_key().to_bytes()),
            delegation: b64(&delegation.to_bytes()),
            sig: b64(&signature.to_bytes()),
            counter,
        }
    }

    fn notify_request(gateway: &SigningKey, device_id: &str, counter: u64) -> NotifyRequest {
        let collapse_key = "collapse";
        let enc = "Y2lwaGVydGV4dA==";
        let nonce = "bm9uY2U=";
        let signature = test_sign::sign_notify(
            gateway,
            &remote_host_protocol::push::NotifySigningInput {
                device_id,
                collapse_key,
                enc,
                n: nonce,
                bid: device_id,
                counter,
            },
        );
        NotifyRequest {
            device_id: device_id.into(),
            collapse_key: collapse_key.into(),
            bid: device_id.into(),
            enc: enc.into(),
            n: nonce.into(),
            sig: b64(&signature.to_bytes()),
            counter,
        }
    }

    fn make_service(
        provider: Arc<MockProvider>,
        store: Arc<InMemoryDeviceTokenStore>,
        traffic: Arc<PushTrafficRegistry>,
    ) -> NotifyService {
        let providers = PushProviders::new([provider as Arc<dyn ProviderSender>]);
        NotifyService::new(
            store,
            Arc::new(providers),
            traffic,
            NotifyRateLimiter::default(),
        )
    }

    fn seed(store: &InMemoryDeviceTokenStore, gateway: &SigningKey, device_id: &str) {
        assert!(store.register(
            device_id,
            DeviceRegistration {
                target: apns_target(APNS_TOKEN),
                gateway_pubkey: gateway.verifying_key().to_bytes(),
                last_counter: 0,
            },
        ));
    }

    #[test]
    fn register_binds_provider_target_when_chain_verifies() {
        let (device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let provider = Arc::new(MockProvider::new(
            PushProvider::Apns,
            ProviderOutcome::Delivered { provider_id: None },
        ));
        let service = make_service(
            provider,
            Arc::clone(&store),
            Arc::new(PushTrafficRegistry::new()),
        );
        let target = apns_target(APNS_TOKEN);

        assert_eq!(
            service.register(register_request(
                &device,
                &gateway,
                &device_id,
                target.clone(),
                1,
            )),
            RegisterOutcome::Registered
        );
        let stored = store.get(&device_id).unwrap();
        assert_eq!(stored.target, target);
        assert_eq!(stored.last_counter, 1);
    }

    #[test]
    fn register_rejects_target_tampering_and_replay() {
        let (device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let provider = Arc::new(MockProvider::new(
            PushProvider::Apns,
            ProviderOutcome::Delivered { provider_id: None },
        ));
        let service = make_service(
            provider,
            Arc::clone(&store),
            Arc::new(PushTrafficRegistry::new()),
        );
        let mut request =
            register_request(&device, &gateway, &device_id, apns_target(APNS_TOKEN), 1);
        request.target = apns_target("tampered");
        assert_eq!(service.register(request), RegisterOutcome::Rejected);

        let request = register_request(&device, &gateway, &device_id, apns_target(APNS_TOKEN), 2);
        assert_eq!(
            service.register(request.clone()),
            RegisterOutcome::Registered
        );
        assert_eq!(service.register(request), RegisterOutcome::Rejected);
    }

    #[test]
    fn register_rejects_invalid_token_and_unconfigured_provider() {
        let (device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        let provider = Arc::new(MockProvider::new(
            PushProvider::Apns,
            ProviderOutcome::Delivered { provider_id: None },
        ));
        let service = make_service(provider, store, Arc::new(PushTrafficRegistry::new()));
        assert_eq!(
            service.register(register_request(
                &device,
                &gateway,
                &device_id,
                apns_target(""),
                1,
            )),
            RegisterOutcome::InvalidToken
        );
        assert_eq!(
            service.register(register_request(
                &device,
                &gateway,
                &device_id,
                PushTarget::Fcm {
                    token: "fcm-token".into(),
                },
                1,
            )),
            RegisterOutcome::ProviderUnavailable
        );
    }

    #[tokio::test]
    async fn notify_dispatches_encrypted_content_and_records_traffic() {
        let (_device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id);
        let traffic = Arc::new(PushTrafficRegistry::new());
        let provider = Arc::new(MockProvider::new(
            PushProvider::Apns,
            ProviderOutcome::Delivered {
                provider_id: Some("provider-id".into()),
            },
        ));
        let service = make_service(Arc::clone(&provider), store, Arc::clone(&traffic));

        assert_eq!(
            service
                .notify(notify_request(&gateway, &device_id, 1), 1000)
                .await,
            NotifyOutcome::Delivered
        );
        let sent = provider.last.lock().clone().unwrap();
        assert_eq!(sent.collapse_key, "collapse");
        assert_eq!(sent.enc, "Y2lwaGVydGV4dA==");
        assert_eq!(traffic.snapshot()[0].counts.bytes, 123);
    }

    #[tokio::test]
    async fn notify_rejects_foreign_signature_and_replay() {
        let (_device, gateway, device_id) = keys();
        let impostor = test_sign::signing_key(9);
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id);
        let provider = Arc::new(MockProvider::new(
            PushProvider::Apns,
            ProviderOutcome::Delivered { provider_id: None },
        ));
        let service = make_service(provider, store, Arc::new(PushTrafficRegistry::new()));

        assert_eq!(
            service
                .notify(notify_request(&impostor, &device_id, 1), 1000)
                .await,
            NotifyOutcome::Rejected
        );
        assert_eq!(
            service
                .notify(notify_request(&gateway, &device_id, 1), 1000)
                .await,
            NotifyOutcome::Delivered
        );
        assert_eq!(
            service
                .notify(notify_request(&gateway, &device_id, 1), 1000)
                .await,
            NotifyOutcome::Rejected
        );
    }

    #[tokio::test]
    async fn invalid_token_prunes_but_transient_error_keeps_binding() {
        let (_device, gateway, device_id) = keys();
        let store = Arc::new(InMemoryDeviceTokenStore::new());
        seed(&store, &gateway, &device_id);
        let provider = Arc::new(MockProvider::new(
            PushProvider::Apns,
            ProviderOutcome::InvalidToken {
                reason: "dead".into(),
            },
        ));
        let service = make_service(
            provider,
            Arc::clone(&store),
            Arc::new(PushTrafficRegistry::new()),
        );
        assert_eq!(
            service
                .notify(notify_request(&gateway, &device_id, 1), 1000)
                .await,
            NotifyOutcome::Pruned
        );
        assert!(store.get(&device_id).is_none());

        seed(&store, &gateway, &device_id);
        let provider = Arc::new(MockProvider::new(
            PushProvider::Apns,
            ProviderOutcome::TransientError("503".into()),
        ));
        let service = make_service(
            provider,
            Arc::clone(&store),
            Arc::new(PushTrafficRegistry::new()),
        );
        assert_eq!(
            service
                .notify(notify_request(&gateway, &device_id, 1), 1000)
                .await,
            NotifyOutcome::Failed("503".into())
        );
        assert!(store.get(&device_id).is_some());
    }
}
