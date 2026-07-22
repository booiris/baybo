//! Vault-backed accessors for the OAuth token bundle.

use std::sync::Arc;

use baybo_security::SecretVault;
use baybo_store::StoreIdentity;

use super::VAULT_KEY_TOKENS;
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
}
