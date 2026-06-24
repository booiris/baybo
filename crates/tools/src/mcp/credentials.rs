//! `CredentialStore` impl that persists rmcp's `StoredCredentials` into
//! `SecretVault`, so refreshed tokens automatically land back on disk
//! without an explicit re-auth.
//!
//! Each MCP server's credentials live under a single vault key
//! (`mcp.<name>.oauth.credentials`) carrying the JSON-serialized
//! [`StoredCredentials`] (client_id + access_token + refresh_token +
//! granted_scopes + token_received_at). When `AuthClient` triggers a
//! refresh, `AuthorizationManager` calls `save()` on this store with
//! the rotated credentials — so the next process start picks up the
//! new tokens without losing them.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_security::SecretVault;
use rmcp::transport::auth::{AuthError, CredentialStore, StoredCredentials};

use crate::mcp::vault_keys;

pub struct VaultCredentialStore {
    vault: Arc<SecretVault>,
    key: String,
}

impl VaultCredentialStore {
    pub fn new(vault: Arc<SecretVault>, server_name: &str) -> Self {
        Self {
            key: vault_keys::oauth_credentials(server_name),
            vault,
        }
    }
}

#[async_trait]
impl CredentialStore for VaultCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let secret = self
            .vault
            .get_secret(&self.key)
            .await
            .map_err(|e| AuthError::InternalError(format!("vault read: {e}")))?;
        let Some(secret) = secret else {
            return Ok(None);
        };
        let stored: StoredCredentials = serde_json::from_slice(secret.as_bytes())
            .map_err(|e| AuthError::InternalError(format!("decode credentials: {e}")))?;
        Ok(Some(stored))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(&credentials)
            .map_err(|e| AuthError::InternalError(format!("encode credentials: {e}")))?;
        self.vault
            .store_secret(&self.key, &bytes)
            .await
            .map_err(|e| AuthError::InternalError(format!("vault write: {e}")))?;
        Ok(())
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.vault
            .delete_secret(&self.key)
            .await
            .map_err(|e| AuthError::InternalError(format!("vault delete: {e}")))?;
        Ok(())
    }
}
