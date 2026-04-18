//! Builders and constructors for end-to-end tests.

use std::sync::Arc;

use aura_agent::SecurityGateway;
use aura_model::{ChannelType, ChatMessage, Session, SessionState, User};
use aura_security::{EncryptionKey, LeakDetector, SecretVault};
use aura_storage::test_support::MemorySecretStore;
use chrono::Utc;

/// Stable 32-byte key used by every fixture so placeholder hex values
/// stay reproducible across runs and across test binaries.
pub fn master_key_for_tests() -> EncryptionKey {
    EncryptionKey::new(b"aura-it-master-key-32-bytes!!!!!".to_vec())
        .expect("32-byte test master key")
}

/// Build a fresh `SecurityGateway` backed by an in-memory secret store.
/// Returns the gateway plus a handle to the store so tests can assert
/// on vault state (e.g. "exactly one entry minted").
pub fn gateway_with_memory_vault() -> (Arc<SecurityGateway>, Arc<MemorySecretStore>, Arc<SecretVault>)
{
    let detector = Arc::new(LeakDetector::with_default_rules());
    let store = Arc::new(MemorySecretStore::new());
    let vault = Arc::new(SecretVault::new(
        master_key_for_tests(),
        store.clone() as Arc<dyn aura_storage::SecretStore>,
    ));
    let gateway = Arc::new(SecurityGateway::new(detector, vault.clone()));
    (gateway, store, vault)
}

/// Builder for `Session` so tests don't repeat the field list.
///
/// Defaults: id `"sess-it"`, user `"user-it"` on `ChannelType::Tui`,
/// no messages, `created_at == last_active == Utc::now()`, default
/// `SessionState`. Override only what the test cares about.
pub struct SessionBuilder {
    id: String,
    user_id: String,
    user_name: Option<String>,
    channel: ChannelType,
    messages: Vec<ChatMessage>,
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            id: "sess-it".into(),
            user_id: "user-it".into(),
            user_name: Some("integration-test-user".into()),
            channel: ChannelType::Tui,
            messages: Vec::new(),
        }
    }
}

impl SessionBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn channel(mut self, channel: ChannelType) -> Self {
        self.channel = channel;
        self
    }

    pub fn user(mut self, id: impl Into<String>, name: Option<String>) -> Self {
        self.user_id = id.into();
        self.user_name = name;
        self
    }

    pub fn messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.messages = messages;
        self
    }

    pub fn build(self) -> Session {
        let now = Utc::now();
        Session {
            id: self.id,
            user: User {
                id: self.user_id,
                name: self.user_name,
                channel: self.channel,
            },
            channel: self.channel,
            messages: self.messages,
            created_at: now,
            last_active: now,
            state: SessionState::default(),
        }
    }
}
