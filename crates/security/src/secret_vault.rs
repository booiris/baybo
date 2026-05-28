//! In-process encrypted secret storage. Owns the master key, encrypts
//! plaintext with AES-256-GCM, and delegates persistence to a
//! [`SecretStore`] implementation injected by the assembly layer.

use std::sync::Arc;

use crate::crypto::{self, EncryptionKey};
use crate::secret_value::SecretValue;
use crate::{Result, SecurityError};
use aura_store::SecretStore;

pub struct SecretVault {
    master_key: EncryptionKey,
    store: Arc<dyn SecretStore>,
}

impl SecretVault {
    pub fn new(master_key: EncryptionKey, store: Arc<dyn SecretStore>) -> Self {
        Self { master_key, store }
    }

    /// Access the master encryption key. Intended for subsystems (like
    /// `PlaceholderMinter`) that need to derive subkeys from it.
    pub fn master_key(&self) -> &EncryptionKey {
        &self.master_key
    }

    pub async fn store_secret(&self, name: &str, value: &[u8]) -> Result<()> {
        let encrypted = crypto::encrypt(value, &self.master_key)?;
        self.store
            .store(name, &encrypted)
            .await
            .map_err(|e| SecurityError::Storage(e.to_string()))
    }

    pub async fn get_secret(&self, name: &str) -> Result<Option<SecretValue>> {
        let encrypted = self
            .store
            .retrieve(name)
            .await
            .map_err(|e| SecurityError::Storage(e.to_string()))?;
        match encrypted {
            Some(data) => {
                let plaintext = crypto::decrypt(&data, &self.master_key)?;
                Ok(Some(SecretValue::new(plaintext)))
            }
            None => Ok(None),
        }
    }

    pub async fn delete_secret(&self, name: &str) -> Result<()> {
        self.store
            .delete(name)
            .await
            .map_err(|e| SecurityError::Storage(e.to_string()))
    }

    /// All vault entry names, without decrypting any value. Higher layers that
    /// namespace their keys (e.g. `UserSecretManager`) filter by prefix.
    pub async fn list_names(&self) -> Result<Vec<String>> {
        self.store
            .list()
            .await
            .map_err(|e| SecurityError::Storage(e.to_string()))
    }

    /// Store any `serde::Serialize` value as JSON inside an encrypted vault
    /// entry. Same AES-GCM encryption as `store_secret`; just adds a JSON
    /// envelope so callers don't have to hand-roll bytes for typed payloads
    /// like OAuth bundles.
    pub async fn store_typed<T: serde::Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| SecurityError::Storage(format!("serialize {name}: {e}")))?;
        self.store_secret(name, &bytes).await
    }

    /// Counterpart to `store_typed`. Returns `None` when the entry is absent;
    /// returns `SecurityError::Storage` when bytes exist but JSON-decode
    /// fails (callers typically treat that as "corrupt — clear and re-init").
    pub async fn get_typed<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let Some(secret) = self.get_secret(name).await? else {
            return Ok(None);
        };
        let value: T = serde_json::from_slice(secret.as_bytes())
            .map_err(|e| SecurityError::Storage(format!("deserialize {name}: {e}")))?;
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemorySecretStore;

    fn make_vault() -> SecretVault {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        SecretVault::new(key, Arc::new(MemorySecretStore::new()))
    }

    #[tokio::test]
    async fn store_and_retrieve_secret() {
        let vault = make_vault();
        vault
            .store_secret("my_api_key", b"super-secret-value")
            .await
            .unwrap();
        let secret = vault.get_secret("my_api_key").await.unwrap().unwrap();
        assert_eq!(secret.as_bytes(), b"super-secret-value");
    }

    #[tokio::test]
    async fn missing_secret_returns_none() {
        let vault = make_vault();
        let result = vault.get_secret("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn typed_round_trip() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Bundle {
            access: String,
            count: u32,
        }
        let vault = make_vault();
        let original = Bundle {
            access: "tok-abc".into(),
            count: 7,
        };
        vault.store_typed("bundle", &original).await.unwrap();
        let recovered: Bundle = vault.get_typed("bundle").await.unwrap().unwrap();
        assert_eq!(recovered, original);
    }

    #[tokio::test]
    async fn typed_missing_returns_none() {
        let vault = make_vault();
        let recovered: Option<String> = vault.get_typed("nope").await.unwrap();
        assert!(recovered.is_none());
    }

    #[tokio::test]
    async fn typed_corrupt_payload_errors() {
        // Stored as raw bytes that don't parse as JSON; get_typed must fail
        // loudly so the caller can treat it as "corrupt — clear and re-init".
        let vault = make_vault();
        vault
            .store_secret("garbage", b"not-json-at-all")
            .await
            .unwrap();
        let err = vault.get_typed::<u32>("garbage").await.unwrap_err();
        match err {
            SecurityError::Storage(msg) => assert!(msg.contains("deserialize garbage")),
            other => panic!("expected SecurityError::Storage, got {other:?}"),
        }
    }
}
