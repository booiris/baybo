//! Builders and constructors for end-to-end tests.

use std::sync::Arc;

use baybo_agent::SecurityGateway;
use baybo_model::{ChannelType, Session, SessionId, SessionState, TriggerSource, User};
use baybo_security::test_support::MemorySecretStore;
use baybo_security::{EncryptionKey, LeakDetector, SecretVault};
use chrono::Utc;

/// Stable 32-byte key used by every fixture so placeholder hex values
/// stay reproducible across runs and across test binaries.
pub fn master_key_for_tests() -> EncryptionKey {
    EncryptionKey::new(b"baybo-it-master-key-32-bytes!!!!".to_vec())
        .expect("32-byte test master key")
}

/// Build a fresh `SecurityGateway` backed by an in-memory secret store.
/// Returns the gateway plus a handle to the store so tests can assert
/// on vault state (e.g. "exactly one entry minted").
pub fn gateway_with_memory_vault() -> (
    Arc<SecurityGateway>,
    Arc<MemorySecretStore>,
    Arc<SecretVault>,
) {
    let detector = Arc::new(LeakDetector::with_default_rules());
    let store = Arc::new(MemorySecretStore::new());
    let vault = Arc::new(SecretVault::new(
        master_key_for_tests(),
        store.clone() as Arc<dyn baybo_store::SecretStore>,
    ));
    let gateway = Arc::new(SecurityGateway::new(detector, vault.clone()));
    (gateway, store, vault)
}

/// Builder for `Session` so tests don't repeat the field list.
///
/// Defaults: id `"sess-it"`, user `"user-it"` on `ChannelType::tui()`,
/// `created_at == last_active == Utc::now()`, default `SessionState`.
/// Override only what the test cares about. Conversation transcripts
/// now live on `baybo_context::ContextManager`; tests that need
/// pre-seeded messages should call `ContextManager::restore_messages`
/// separately.
pub struct SessionBuilder {
    id: String,
    user_id: String,
    user_name: Option<String>,
    channel: ChannelType,
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            id: "sess-it".into(),
            user_id: "user-it".into(),
            user_name: Some("integration-test-user".into()),
            channel: ChannelType::tui(),
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

    pub fn build(self) -> Session {
        let now = Utc::now();
        let id = SessionId::from(self.id);
        Session {
            id: id.clone(),
            user: User {
                id: self.user_id,
                name: self.user_name,
                channel: self.channel.clone(),
            },
            channel: self.channel,
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: id,
            trigger: TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            folder_id: None,
        }
    }
}
