//! Vault-backed accessors for the OAuth token bundle.

use std::sync::Arc;

use baybo_security::SecretVault;
use baybo_store::StoreIdentity;

use super::VAULT_KEY_TOKENS;
use super::oauth::Registration;
const HOST_KEY: &str = "llm.openai-subscription.host";
const REGISTRATION_KEY: &str = "llm.openai-subscription.registration";
use super::token_bundle::OAuthTokenBundle;
use crate::{LlmError, Result};

/// Identity of the credential a [`VaultTokenStore`] reads and writes: which
/// secret store, which entry inside it. Two stores with equal keys are two
/// handles on ONE OAuth bundle and so MUST share refresh state — that is
/// what makes the spec's process-wide single-flight guarantee hold across
/// every client built from the same credential.
///
/// Keyed on the STORE, not on the vault handle: two `SecretVault` instances
/// over one database are two views of the same credential, and keying on
/// the handle would hand them separate coordinators — reinstating the race
/// this type exists to prevent. It also gives cross-process coordination a
/// filesystem anchor (see `StoreIdentity::file_path`).
///
/// `vault_key` is [`VAULT_KEY_TOKENS`] for every store today (single profile
/// per process). It is a field rather than an implied constant so the
/// deferred multi-profile work shards the coordinator map by construction
/// instead of needing a redesign.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CredentialKey {
    store: StoreIdentity,
    vault_key: &'static str,
}

impl CredentialKey {
    /// Filesystem anchor for cross-process locking, when the credential has
    /// one. In-memory stores return `None` — nothing outside this process
    /// can reach them.
    pub(super) fn lock_path(&self) -> Option<std::path::PathBuf> {
        let store = self.store.file_path()?;
        // Resolve here rather than trusting the identity: it may have been
        // minted before the store file existed (so only its parent could be
        // canonicalized), and two processes that reached the same store
        // through different symlinks must still name ONE lock file.
        let store = std::fs::canonicalize(store).unwrap_or_else(|_| store.to_path_buf());
        let mut name = store.file_name().unwrap_or_default().to_os_string();
        name.push(".");
        name.push(self.vault_key);
        name.push(".refresh.lock");
        Some(store.with_file_name(name))
    }
}

#[derive(Clone)]
pub struct VaultTokenStore {
    vault: Arc<SecretVault>,
}

impl VaultTokenStore {
    pub fn new(vault: Arc<SecretVault>) -> Self {
        Self { vault }
    }

    pub(super) fn credential_key(&self) -> CredentialKey {
        CredentialKey {
            store: self.vault.store_identity(),
            vault_key: VAULT_KEY_TOKENS,
        }
    }

    pub(super) async fn host_id(&self) -> Result<String> {
        static HOST_INIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = HOST_INIT.lock().await;
        let path = self.credential_key().lock_path();
        let file_guard = super::refresh_coordinator::lock_credential(path.clone()).await;
        if path.is_some() && file_guard.is_none() {
            return Err(LlmError::Config(
                "ChatGPT host initialization could not acquire the credential lock".into(),
            ));
        }

        if let Some(id) = self
            .vault
            .get_typed::<String>(HOST_KEY)
            .await
            .map_err(|e| LlmError::Config(format!("ChatGPT host read: {e}")))?
        {
            return Ok(id);
        }
        let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        self.vault
            .store_typed(HOST_KEY, &id)
            .await
            .map_err(|e| LlmError::Config(format!("ChatGPT host write: {e}")))?;
        Ok(id)
    }

    pub(super) async fn registration(&self) -> Result<Option<Registration>> {
        self.vault
            .get_typed(REGISTRATION_KEY)
            .await
            .map_err(|e| LlmError::Config(format!("ChatGPT registration read: {e}")))
    }

    pub(super) async fn save_registration(&self, registration: &Registration) -> Result<()> {
        self.vault
            .store_typed(REGISTRATION_KEY, registration)
            .await
            .map_err(|e| LlmError::Config(format!("ChatGPT registration write: {e}")))
    }

    pub async fn load(&self) -> Result<Option<OAuthTokenBundle>> {
        self.vault
            .get_typed::<OAuthTokenBundle>(VAULT_KEY_TOKENS)
            .await
            .map_err(|e| LlmError::Config(format!("openai-subscription: vault read failed: {e}")))
    }

    pub async fn save(&self, bundle: &OAuthTokenBundle) -> Result<()> {
        self.vault
            .store_typed(VAULT_KEY_TOKENS, bundle)
            .await
            .map_err(|e| LlmError::Config(format!("openai-subscription: vault write failed: {e}")))
    }

    pub(super) async fn save_login(
        &self,
        bundle: &OAuthTokenBundle,
        http: &reqwest::Client,
    ) -> Result<()> {
        use super::refresh_coordinator::{BackgroundRefresh, RefreshCoordinator};
        RefreshCoordinator::shared(self.clone(), http.clone(), BackgroundRefresh::Disabled)
            .replace_credentials(bundle)
            .await
    }

    pub async fn clear(&self) -> Result<()> {
        self.vault
            .delete_secret(VAULT_KEY_TOKENS)
            .await
            .map_err(|e| LlmError::Config(format!("openai-subscription: vault delete failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_security::EncryptionKey;
    use baybo_security::test_support::MemorySecretStore;
    use std::sync::Arc;

    fn make_store() -> VaultTokenStore {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let vault = Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())));
        VaultTokenStore::new(vault)
    }

    #[tokio::test]
    async fn load_returns_none_when_empty() {
        let store = make_store();
        assert!(store.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn save_then_load_round_trip() {
        let store = make_store();
        let bundle = OAuthTokenBundle {
            connection: None,
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: "it".into(),
            account_id: Some("acc-1".into()),
            expires_at: 1000,
            obtained_at: 500,
        };
        store.save(&bundle).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded, bundle);
    }

    #[tokio::test]
    async fn clear_removes_entry() {
        let store = make_store();
        let bundle = OAuthTokenBundle {
            connection: None,
            access_token: "x".into(),
            refresh_token: "x".into(),
            id_token: "x".into(),
            account_id: None,
            expires_at: 0,
            obtained_at: 0,
        };
        store.save(&bundle).await.unwrap();
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }
    #[tokio::test]
    async fn concurrent_host_initialization_and_logout_retain_the_same_installation() {
        let store = make_store();
        let peer = store.clone();
        let (first, concurrent) = tokio::join!(store.host_id(), peer.host_id());
        let host = first.unwrap();
        assert!(host.starts_with("urn:uuid:"));
        assert_eq!(host, concurrent.unwrap());
        let registration = Registration {
            client_id: "oaiapp_test".into(),
            subject: Some("user".into()),
        };
        store.save_registration(&registration).await.unwrap();
        store.clear().await.unwrap();
        assert_eq!(store.host_id().await.unwrap(), host);
        assert_eq!(store.registration().await.unwrap(), Some(registration));
    }

    #[tokio::test]
    async fn legacy_bundles_without_connection_remain_readable() {
        let legacy = serde_json::json!({
            "access_token": "a", "refresh_token": "r", "id_token": "i",
            "account_id": null, "expires_at": 1000, "obtained_at": 500
        });
        let bundle: OAuthTokenBundle = serde_json::from_value(legacy).unwrap();
        assert!(bundle.connection.is_none());
        let store = make_store();
        store.save(&bundle).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(bundle));
    }
    #[tokio::test]
    async fn login_replaces_a_cached_legacy_bundle_even_with_a_shorter_expiry() {
        use super::super::refresh_coordinator::{BackgroundRefresh, RefreshCoordinator};
        use super::super::token_bundle::ChatGptConnection;
        let store = make_store();
        let now = chrono::Utc::now().timestamp();
        let old = OAuthTokenBundle {
            connection: None,
            access_token: "old".into(),
            refresh_token: "old-refresh".into(),
            id_token: "old-id".into(),
            account_id: None,
            expires_at: now + 7200,
            obtained_at: now,
        };
        store.save(&old).await.unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let coordinator =
            RefreshCoordinator::shared(store.clone(), http.clone(), BackgroundRefresh::Disabled);
        assert_eq!(
            coordinator
                .ensure_fresh_bundle()
                .await
                .unwrap()
                .access_token,
            "old"
        );
        let mut new = old.clone();
        new.connection = Some(ChatGptConnection {
            client_id: "oaiapp_test".into(),
            subject: "user".into(),
            scopes: vec![super::super::oauth::SHARING_SCOPE.into()],
        });
        new.access_token = "new".into();
        new.expires_at = now + 3600;
        store.save_login(&new, &http).await.unwrap();
        assert_eq!(*coordinator.ensure_fresh_bundle().await.unwrap(), new);
    }
}
