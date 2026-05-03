//! Vault-backed accessors for the OAuth token bundle.

use std::sync::Arc;

use aura_security::SecretVault;

use super::VAULT_KEY_TOKENS;
use super::token_bundle::OAuthTokenBundle;
use crate::{LlmError, Result};

#[derive(Clone)]
pub struct VaultTokenStore {
    vault: Arc<SecretVault>,
}

impl VaultTokenStore {
    pub fn new(vault: Arc<SecretVault>) -> Self {
        Self { vault }
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
    use aura_security::EncryptionKey;
    use aura_storage::test_support::MemorySecretStore;
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
