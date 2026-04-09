use std::collections::HashMap;
use std::sync::Arc;

use crate::Result;

use crate::SecretStore;
use crate::crypto::{self, EncryptionKey};

/// An in-process wrapper that holds a master encryption key and delegates
/// encrypted persistence to a [`SecretStore`] implementation.
///
/// The master key is never written to persistent storage.
pub struct SecretVault {
    master_key: EncryptionKey,
    store: Arc<dyn SecretStore>,
}

/// A secret value wrapper that prevents plaintext from appearing in `Debug`
/// output, logs, or serialized forms.
#[derive(Clone)]
pub struct SecretValue {
    inner: Vec<u8>,
}

impl SecretValue {
    /// Create a new `SecretValue` from raw bytes.
    pub fn new(value: Vec<u8>) -> Self {
        Self { inner: value }
    }

    /// Access the plaintext bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    /// Attempt to interpret the value as a UTF-8 string.
    pub fn as_str(&self) -> Result<&str> {
        std::str::from_utf8(&self.inner).map_err(|e| {
            crate::SecurityError::Encryption(format!("secret is not valid UTF-8: {e}"))
        })
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl std::fmt::Display for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl SecretVault {
    /// Create a new vault with the given master key and backing store.
    pub fn new(master_key: EncryptionKey, store: Arc<dyn SecretStore>) -> Self {
        Self { master_key, store }
    }

    /// Encrypt and store a secret under the given name.
    pub async fn store_secret(&self, name: &str, value: &[u8]) -> Result<()> {
        let encrypted = crypto::encrypt(value, &self.master_key)?;
        self.store.store(name, &encrypted).await
    }

    /// Retrieve and decrypt a secret by name.
    /// Returns `Ok(None)` if the secret does not exist.
    pub async fn get_secret(&self, name: &str) -> Result<Option<SecretValue>> {
        let encrypted = self.store.retrieve(name).await?;
        match encrypted {
            Some(data) => {
                let plaintext = crypto::decrypt(&data, &self.master_key)?;
                Ok(Some(SecretValue::new(plaintext)))
            }
            None => Ok(None),
        }
    }

    /// Retrieve only the secrets that a tool has explicitly declared as
    /// required. Returns a map of secret name to decrypted `SecretValue`.
    ///
    /// Secrets that are declared but not found in the store are silently
    /// omitted from the result (the tool should handle missing secrets).
    pub async fn get_secrets_for_tool(
        &self,
        _tool_name: &str,
        declared: &[String],
    ) -> Result<HashMap<String, SecretValue>> {
        let mut result = HashMap::new();
        for name in declared {
            if let Some(value) = self.get_secret(name).await? {
                result.insert(name.clone(), value);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
impl SecretVault {
    async fn delete_secret(&self, name: &str) -> Result<()> {
        self.store.delete(name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretStore;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Simple in-memory SecretStore for testing.
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

    #[async_trait::async_trait]
    impl SecretStore for MemorySecretStore {
        async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()> {
            self.data
                .lock()
                .map_err(|e| crate::SecurityError::Storage(e.to_string()))?
                .insert(name.to_owned(), encrypted_value.to_vec());
            Ok(())
        }

        async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>> {
            Ok(self
                .data
                .lock()
                .map_err(|e| crate::SecurityError::Storage(e.to_string()))?
                .get(name)
                .cloned())
        }

        async fn delete(&self, name: &str) -> Result<()> {
            self.data
                .lock()
                .map_err(|e| crate::SecurityError::Storage(e.to_string()))?
                .remove(name);
            Ok(())
        }

        async fn list(&self) -> Result<Vec<String>> {
            Ok(self
                .data
                .lock()
                .map_err(|e| crate::SecurityError::Storage(e.to_string()))?
                .keys()
                .cloned()
                .collect())
        }
    }

    fn make_vault() -> SecretVault {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let store = Arc::new(MemorySecretStore::new());
        SecretVault::new(key, store)
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

    #[tokio::test]
    async fn get_secrets_for_tool_returns_only_declared() {
        let vault = make_vault();
        vault.store_secret("secret_a", b"aaa").await.unwrap();
        vault.store_secret("secret_b", b"bbb").await.unwrap();
        vault.store_secret("secret_c", b"ccc").await.unwrap();

        let declared = vec!["secret_a".to_owned(), "secret_c".to_owned()];
        let result = vault
            .get_secrets_for_tool("some_tool", &declared)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert!(result.contains_key("secret_a"));
        assert!(result.contains_key("secret_c"));
        assert!(!result.contains_key("secret_b"));
    }

    #[test]
    fn secret_value_debug_is_redacted() {
        let sv = SecretValue::new(b"my-password".to_vec());
        let debug = format!("{sv:?}");
        assert_eq!(debug, "[REDACTED]");
        assert!(!debug.contains("my-password"));
    }

    #[test]
    fn secret_value_display_is_redacted() {
        let sv = SecretValue::new(b"my-password".to_vec());
        let display = format!("{sv}");
        assert_eq!(display, "[REDACTED]");
    }
}
