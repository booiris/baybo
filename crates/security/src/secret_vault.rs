//! In-process encrypted secret storage. Owns the master key, encrypts
//! plaintext with AES-256-GCM, and delegates persistence to a
//! [`SecretStore`] implementation injected by the assembly layer.

use std::sync::Arc;

use crate::crypto::{self, EncryptionKey};
use crate::secret_value::SecretValue;
use crate::{Result, SecurityError};
use baybo_store::{SecretStore, StoreIdentity};

pub struct SecretVault {
    master_key: EncryptionKey,
    store: Arc<dyn SecretStore>,
}

impl SecretVault {
    pub fn new(master_key: EncryptionKey, store: Arc<dyn SecretStore>) -> Self {
        Self { master_key, store }
    }

    /// Which credential set this vault addresses. Subsystems that keep
    /// per-credential coordination state key it on this rather than on the
    /// vault handle — two vaults over one store are one credential, and
    /// treating them as two is what lets two coordinators race a refresh.
    pub fn store_identity(&self) -> StoreIdentity {
        self.store.identity()
    }

    /// Access the master encryption key. Intended for subsystems (like
    /// `PlaceholderMinter`) that need to derive subkeys from it.
    pub fn master_key(&self) -> &EncryptionKey {
        &self.master_key
    }

    /// The entry name is the associated data, so a record only decrypts under
    /// the name it was written for. Moving `llm.entry.cheap.api_key`'s
    /// ciphertext onto `gateway.admin_token` no longer yields a usable value.
    pub async fn store_secret(&self, name: &str, value: &[u8]) -> Result<()> {
        let encrypted = crypto::encrypt(value, &self.master_key, name.as_bytes())?;
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
                let plaintext = crypto::decrypt(&data, &self.master_key, name.as_bytes())?;
                Ok(Some(SecretValue::new(plaintext)))
            }
            None => Ok(None),
        }
    }

    /// Every entry's name and **still-encrypted** value.
    ///
    /// Exists so a caller can snapshot the vault without holding the plaintext:
    /// paired with the key file that was live at the time, this is everything
    /// needed to put the vault back, and nothing more. Rotation is the only
    /// caller.
    pub async fn export_encrypted(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let names = self.list_names().await?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let encrypted = self
                .store
                .retrieve(&name)
                .await
                .map_err(|e| SecurityError::Storage(e.to_string()))?
                .ok_or_else(|| SecurityError::Storage(format!("entry {name} vanished")))?;
            out.push((name, encrypted));
        }
        Ok(out)
    }

    /// Re-encrypt every entry under `new_key`, atomically, and report how many
    /// moved.
    ///
    /// The caller owns the key file: this touches only the store, so on success
    /// the ciphertext is under `new_key` while the key on disk is still the old
    /// one. Promoting the key file is the caller's next step, and the ordering
    /// matters — see `baybo-setup`'s rotation, which writes the new key to a
    /// pending path *before* calling this so an interrupted rotation is
    /// recoverable from either side.
    ///
    /// Nothing else may hold this vault while it runs. A concurrent writer's
    /// entry is encrypted under the old key and is not in the snapshot this
    /// re-encrypts, so it would be silently unreadable afterwards.
    pub async fn rotate_master_key(&self, new_key: &EncryptionKey) -> Result<usize> {
        let names = self.list_names().await?;
        let mut rewritten = Vec::with_capacity(names.len());
        for name in &names {
            let encrypted = self
                .store
                .retrieve(name)
                .await
                .map_err(|e| SecurityError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    SecurityError::Storage(format!("entry {name} vanished mid-rotation"))
                })?;
            let plaintext = crypto::decrypt(&encrypted, &self.master_key, name.as_bytes())?;
            rewritten.push((
                name.clone(),
                crypto::encrypt(&plaintext, new_key, name.as_bytes())?,
            ));
        }
        self.store
            .rewrite_all(&rewritten)
            .await
            .map_err(|e| SecurityError::Storage(e.to_string()))?;
        Ok(rewritten.len())
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
