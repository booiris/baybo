//! User-managed environment-variable-style secrets layered over
//! [`SecretVault`]. Each secret is stored under a reserved `user_env.<NAME>`
//! namespace so it never collides with auto-minted placeholder entries or the
//! `mcp.<name>.…` credential bags. This is the single source of truth for the
//! prefix, name validation, and masked previews shared by the CLI and the
//! agent's tool-side secret access.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::secret_value::SecretValue;
use crate::secret_vault::SecretVault;
use crate::{Result, SecurityError};

/// Reserved vault-key namespace for user-managed secrets. A user secret named
/// `FOO` is stored at `user_env.FOO`. Env var names cannot contain `.`, and
/// every internal vault key either contains `.` (`mcp.*`, `baybo.*`) or is a
/// placeholder string, so this namespace cannot collide.
pub const USER_SECRET_PREFIX: &str = "user_env.";

/// How many leading/trailing chars of a value a `list` preview reveals.
const PREVIEW_KEEP: usize = 4;

/// Whether an [`UserSecretManager::add`] created a new entry or replaced one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Created,
    Replaced,
}

/// Policy layer over [`SecretVault`] for user-managed secrets.
pub struct UserSecretManager {
    vault: Arc<SecretVault>,
}

impl UserSecretManager {
    pub fn new(vault: Arc<SecretVault>) -> Self {
        Self { vault }
    }

    /// Validate a user-supplied secret name as a POSIX-style env var name:
    /// `[A-Za-z_][A-Za-z0-9_]*`. Case is preserved (not upper-cased).
    pub fn validate_name(name: &str) -> Result<()> {
        let mut chars = name.chars();
        let ok = matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
        if ok {
            Ok(())
        } else {
            Err(SecurityError::Violation(format!(
                "invalid secret name '{name}': must match [A-Za-z_][A-Za-z0-9_]*"
            )))
        }
    }

    fn vault_key(name: &str) -> String {
        format!("{USER_SECRET_PREFIX}{name}")
    }

    /// Store a secret. Validates the name and rejects empty values. When the
    /// name already exists and `overwrite` is false, returns a `Violation`
    /// rather than clobbering the stored value.
    pub async fn add(&self, name: &str, value: &[u8], overwrite: bool) -> Result<AddOutcome> {
        Self::validate_name(name)?;
        if value.is_empty() {
            return Err(SecurityError::Violation(
                "secret value must not be empty".to_string(),
            ));
        }
        let existed = self.exists(name).await?;
        if existed && !overwrite {
            return Err(SecurityError::Violation(format!(
                "secret '{name}' already exists; pass overwrite to replace it"
            )));
        }
        self.vault
            .store_secret(&Self::vault_key(name), value)
            .await?;
        Ok(if existed {
            AddOutcome::Replaced
        } else {
            AddOutcome::Created
        })
    }

    pub async fn get(&self, name: &str) -> Result<Option<SecretValue>> {
        self.vault.get_secret(&Self::vault_key(name)).await
    }

    pub async fn delete(&self, name: &str) -> Result<()> {
        self.vault.delete_secret(&Self::vault_key(name)).await
    }

    pub async fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.get(name).await?.is_some())
    }

    /// All user-secret names (prefix stripped), sorted.
    pub async fn list(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = self
            .vault
            .list_names()
            .await?
            .into_iter()
            .filter_map(|k| k.strip_prefix(USER_SECRET_PREFIX).map(str::to_string))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Per-name existence for a batch, preserving input order. A single vault
    /// listing covers the whole batch (no decryption).
    pub async fn exists_many(&self, names: &[String]) -> Result<Vec<(String, bool)>> {
        let present: BTreeSet<String> = self.list().await?.into_iter().collect();
        Ok(names
            .iter()
            .map(|n| (n.clone(), present.contains(n)))
            .collect())
    }

    /// `(name, masked_preview)` for every stored secret, sorted by name. The
    /// preview shows only the first/last few chars; short values are fully
    /// masked. CLI-only — never surfaced to the agent.
    pub async fn list_with_previews(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for name in self.list().await? {
            let preview = match self.get(&name).await? {
                Some(v) => mask_preview(v.as_bytes()),
                None => String::new(),
            };
            out.push((name, preview));
        }
        Ok(out)
    }
}

/// Mask a secret value to a `head…tail` preview, or all-`*` when too short to
/// reveal any chars without exposing most of the value.
fn mask_preview(value: &[u8]) -> String {
    let s = String::from_utf8_lossy(value);
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= PREVIEW_KEEP * 2 {
        return "*".repeat(chars.len().max(1));
    }
    let head: String = chars[..PREVIEW_KEEP].iter().collect();
    let tail: String = chars[chars.len() - PREVIEW_KEEP..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::EncryptionKey;
    use crate::test_support::MemorySecretStore;

    fn make_manager() -> (UserSecretManager, Arc<SecretVault>) {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let vault = Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())));
        (UserSecretManager::new(Arc::clone(&vault)), vault)
    }

    #[test]
    fn name_validation() {
        for ok in ["FOO", "_bar", "Api_Key_2", "x"] {
            assert!(UserSecretManager::validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1FOO",
            "has space",
            "has.dot",
            "has-dash",
            "user_env.FOO",
        ] {
            assert!(UserSecretManager::validate_name(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn add_get_round_trip() {
        let (mgr, _) = make_manager();
        assert_eq!(
            mgr.add("OPENAI_API_KEY", b"sk-secret", false)
                .await
                .unwrap(),
            AddOutcome::Created
        );
        let got = mgr.get("OPENAI_API_KEY").await.unwrap().unwrap();
        assert_eq!(got.as_bytes(), b"sk-secret");
    }

    #[tokio::test]
    async fn overwrite_guard() {
        let (mgr, _) = make_manager();
        mgr.add("TOK", b"first", false).await.unwrap();
        assert!(mgr.add("TOK", b"second", false).await.is_err());
        assert_eq!(
            mgr.add("TOK", b"second", true).await.unwrap(),
            AddOutcome::Replaced
        );
        assert_eq!(mgr.get("TOK").await.unwrap().unwrap().as_bytes(), b"second");
    }

    #[tokio::test]
    async fn empty_value_rejected() {
        let (mgr, _) = make_manager();
        assert!(mgr.add("TOK", b"", false).await.is_err());
    }

    #[tokio::test]
    async fn list_strips_prefix_and_ignores_foreign_keys() {
        let (mgr, vault) = make_manager();
        mgr.add("BBB", b"v", false).await.unwrap();
        mgr.add("AAA", b"v", false).await.unwrap();
        // A non-user entry written straight to the vault must not appear.
        vault.store_secret("mcp.server.env", b"v").await.unwrap();
        assert_eq!(mgr.list().await.unwrap(), vec!["AAA", "BBB"]);
    }

    #[tokio::test]
    async fn exists_many_preserves_order() {
        let (mgr, _) = make_manager();
        mgr.add("HAVE", b"v", false).await.unwrap();
        let got = mgr
            .exists_many(&["MISS".to_string(), "HAVE".to_string()])
            .await
            .unwrap();
        assert_eq!(
            got,
            vec![("MISS".to_string(), false), ("HAVE".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn delete_removes() {
        let (mgr, _) = make_manager();
        mgr.add("TOK", b"v", false).await.unwrap();
        mgr.delete("TOK").await.unwrap();
        assert!(!mgr.exists("TOK").await.unwrap());
    }

    #[test]
    fn preview_masks() {
        assert_eq!(mask_preview(b"sk-proj-abcd1234wxyz"), "sk-p…wxyz");
        assert_eq!(mask_preview(b"short"), "*****");
        assert_eq!(mask_preview(b"12345678"), "********");
    }
}
