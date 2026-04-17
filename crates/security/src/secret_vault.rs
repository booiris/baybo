//! In-process encrypted secret storage. Owns the master key, encrypts
//! plaintext with AES-256-GCM, and delegates persistence to a
//! [`SecretStore`] implementation injected by the assembly layer.

use std::sync::Arc;

use aura_storage::SecretStore;

use crate::crypto::{self, EncryptionKey};
use crate::secret_value::SecretValue;
use crate::{Result, SecurityError};

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
}

#[cfg(test)]
impl SecretVault {
    pub(crate) async fn delete_secret(&self, name: &str) -> Result<()> {
        self.store
            .delete(name)
            .await
            .map_err(|e| SecurityError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use aura_storage::StorageError;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MemorySecretStore {
        data: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl MemorySecretStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    fn poison<E: std::fmt::Display>(e: E) -> StorageError {
        StorageError::Storage(format!("mutex poisoned: {e}"))
    }

    #[async_trait]
    impl SecretStore for MemorySecretStore {
        async fn store(
            &self,
            name: &str,
            encrypted_value: &[u8],
        ) -> aura_storage::secret::Result<()> {
            self.data
                .lock()
                .map_err(poison)?
                .insert(name.to_owned(), encrypted_value.to_vec());
            Ok(())
        }
        async fn retrieve(&self, name: &str) -> aura_storage::secret::Result<Option<Vec<u8>>> {
            Ok(self.data.lock().map_err(poison)?.get(name).cloned())
        }
        async fn delete(&self, name: &str) -> aura_storage::secret::Result<()> {
            self.data.lock().map_err(poison)?.remove(name);
            Ok(())
        }
        async fn list(&self) -> aura_storage::secret::Result<Vec<String>> {
            Ok(self.data.lock().map_err(poison)?.keys().cloned().collect())
        }
    }

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
    async fn delete_secret() {
        let vault = make_vault();
        vault.store_secret("temp", b"temporary").await.unwrap();
        vault.delete_secret("temp").await.unwrap();
        assert!(vault.get_secret("temp").await.unwrap().is_none());
    }
}
