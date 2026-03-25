use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use aura_core::Result;
use aura_security::SecretStore;

/// In-memory implementation of [`SecretStore`].
pub struct InMemorySecretStore {
    data: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStore for InMemorySecretStore {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.insert(name.to_string(), encrypted_value.to_vec());
        Ok(())
    }

    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let guard = self.data.read().await;
        Ok(guard.get(name).cloned())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.remove(name);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<String>> {
        let guard = self.data.read().await;
        Ok(guard.keys().cloned().collect())
    }
}
