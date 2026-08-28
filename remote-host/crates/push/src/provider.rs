//! Provider dispatch seam for encrypted push messages.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use remote_host_protocol::push::{PushProvider, PushTarget};

/// Provider-independent content passed to an APNs/FCM adapter. The adapter
/// frames its own platform payload without ever decrypting the fields.
#[derive(Debug, Clone)]
pub struct EncryptedPush {
    pub collapse_key: String,
    pub bid: String,
    pub enc: String,
    pub n: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderOutcome {
    Delivered { provider_id: Option<String> },
    InvalidToken { reason: String },
    TransientError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDelivery {
    pub payload_len: usize,
    pub outcome: ProviderOutcome,
}

#[async_trait]
pub trait ProviderSender: Send + Sync {
    fn provider(&self) -> PushProvider;

    async fn send(&self, target: &PushTarget, message: EncryptedPush, now: u64)
    -> ProviderDelivery;
}

/// Immutable provider registry. Adding FCM is one new [`ProviderSender`]
/// implementation plus one entry at assembly; registration, authorization,
/// storage, throttling, and encrypted-preview handling remain shared.
pub struct PushProviders {
    senders: HashMap<PushProvider, Arc<dyn ProviderSender>>,
}

impl PushProviders {
    pub fn new(senders: impl IntoIterator<Item = Arc<dyn ProviderSender>>) -> Self {
        Self {
            senders: senders
                .into_iter()
                .map(|sender| (sender.provider(), sender))
                .collect(),
        }
    }

    pub fn supports(&self, provider: PushProvider) -> bool {
        self.senders.contains_key(&provider)
    }

    pub async fn send(
        &self,
        target: &PushTarget,
        message: EncryptedPush,
        now: u64,
    ) -> Option<ProviderDelivery> {
        let sender = self.senders.get(&target.provider())?;
        Some(sender.send(target, message, now).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FcmSender;

    #[async_trait]
    impl ProviderSender for FcmSender {
        fn provider(&self) -> PushProvider {
            PushProvider::Fcm
        }

        async fn send(
            &self,
            _target: &PushTarget,
            _message: EncryptedPush,
            _now: u64,
        ) -> ProviderDelivery {
            ProviderDelivery {
                payload_len: 1,
                outcome: ProviderOutcome::Delivered { provider_id: None },
            }
        }
    }

    #[tokio::test]
    async fn registry_routes_by_provider() {
        let providers = PushProviders::new([Arc::new(FcmSender) as Arc<dyn ProviderSender>]);
        let message = EncryptedPush {
            collapse_key: "c".into(),
            bid: "b".into(),
            enc: "e".into(),
            n: "n".into(),
        };
        assert!(
            providers
                .send(&PushTarget::Fcm { token: "t".into() }, message.clone(), 0,)
                .await
                .is_some()
        );
        assert!(
            providers
                .send(
                    &PushTarget::Apns {
                        token: "t".into(),
                        environment: remote_host_protocol::push::ApnsEnvironment::Sandbox,
                    },
                    message,
                    0,
                )
                .await
                .is_none()
        );
    }
}
