//! API-key resolution for LLM entries — the single source of truth
//! for both runtime boot (`crates/baybo/src/boot.rs::build_llm_client`) and the
//! `baybo llm` CLI helpers. Taking primitive args (rather than an
//! `LlmEntry`) avoids a circular dep on `baybo-config`.

use baybo_security::SecretVault;

/// Vault key under which `baybo llm add` stores an entry's literal API
/// key. Single function so the schema can never drift between the
/// writer (`baybo llm add`) and the readers (boot + CLI).
pub fn vault_api_key_name(entry_name: &str) -> String {
    format!("llm.entry.{entry_name}.api_key")
}

/// Resolve an LLM entry's API key. Order:
///   1. Explicit env var (`api_key_env`).
///   2. Per-entry vault key (`llm.entry.<name>.api_key`).
///   3. Provider-specific default env vars (`OPENAI_API_KEY`, …).
///
/// The per-entry vault key sits **above** provider-default env vars
/// so two entries on the same provider can hold distinct credentials
/// without a global env var silently winning over both.
pub async fn resolve_api_key(
    entry_name: &str,
    provider: &str,
    api_key_env: Option<&str>,
    vault: Option<&SecretVault>,
) -> Option<String> {
    if let Some(env) = api_key_env
        && let Ok(v) = std::env::var(env)
    {
        return Some(v);
    }
    if let Some(vault) = vault {
        let key = vault_api_key_name(entry_name);
        if let Ok(Some(secret)) = vault.get_secret(&key).await
            && let Ok(s) = secret.as_str()
        {
            return Some(s.to_string());
        }
    }
    // Last-resort fallback: the provider's factory advertises its
    // conventional env var (e.g. `OPENAI_API_KEY`); we read it from
    // the process env if present. Adding a new provider is enough to
    // extend the fallback — no list to update here.
    crate::default_api_key_env_for_provider(provider).and_then(|env| std::env::var(env).ok())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use baybo_security::EncryptionKey;
    use baybo_security::test_support::MemorySecretStore;

    use super::*;

    fn vault() -> Arc<SecretVault> {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())))
    }

    /// Regression for the adversarial review's "two entries on the
    /// same provider can't have distinct credentials": the per-entry
    /// vault key must beat the provider-default env var.
    #[tokio::test]
    async fn vault_entry_key_wins_over_provider_default_env() {
        struct EnvCleanup(&'static str);
        impl Drop for EnvCleanup {
            fn drop(&mut self) {
                // SAFETY: env mutation is unsynchronised; the var
                // name is unique enough that no other test touches
                // it within this process.
                unsafe { std::env::remove_var(self.0) };
            }
        }
        // SAFETY: see EnvCleanup::drop.
        unsafe { std::env::set_var("OPENAI_API_KEY", "global-default-env-value") };
        let _cleanup = EnvCleanup("OPENAI_API_KEY");

        let v = vault();
        v.store_secret(&vault_api_key_name("alice"), b"alice-vault-secret")
            .await
            .unwrap();

        // alice has no explicit api_key_env — vault must beat the
        // global OPENAI_API_KEY.
        let resolved = resolve_api_key("alice", "openai", None, Some(v.as_ref())).await;
        assert_eq!(resolved.as_deref(), Some("alice-vault-secret"));

        // bob has no vault entry — falls through to the provider
        // default.
        let resolved = resolve_api_key("bob", "openai", None, Some(v.as_ref())).await;
        assert_eq!(resolved.as_deref(), Some("global-default-env-value"));

        // Explicit api_key_env wins over both.
        // SAFETY: see EnvCleanup::drop.
        unsafe { std::env::set_var("BAYBO_TEST_LLM_KEY", "explicit-env-value") };
        let _explicit_cleanup = EnvCleanup("BAYBO_TEST_LLM_KEY");
        let resolved = resolve_api_key(
            "alice",
            "openai",
            Some("BAYBO_TEST_LLM_KEY"),
            Some(v.as_ref()),
        )
        .await;
        assert_eq!(resolved.as_deref(), Some("explicit-env-value"));
    }

    /// Credential rotation: overwriting an entry's vault key is reflected
    /// by the next `resolve_api_key`. This is the property a config
    /// reload's pool rebuild depends on — the rebuild re-resolves from the
    /// vault every time and never caches, so `reload` (which always
    /// rebuilds) and the `baybo llm edit --api-key` → SIGHUP path both pick
    /// up a rotated key on the next reload. The vault key beats the
    /// provider-default env var, so this holds regardless of ambient
    /// `OPENAI_API_KEY`.
    #[tokio::test]
    async fn resolve_reflects_a_rotated_vault_key() {
        let v = vault();
        let key_name = vault_api_key_name("rotato");

        v.store_secret(&key_name, b"old-secret").await.unwrap();
        let before = resolve_api_key("rotato", "openai", None, Some(v.as_ref())).await;
        assert_eq!(before.as_deref(), Some("old-secret"));

        // Rotate the key in place.
        v.store_secret(&key_name, b"new-secret").await.unwrap();
        let after = resolve_api_key("rotato", "openai", None, Some(v.as_ref())).await;
        assert_eq!(
            after.as_deref(),
            Some("new-secret"),
            "a vault rotation must be visible to the next resolve (a reload's rebuild re-resolves)"
        );
    }
}
