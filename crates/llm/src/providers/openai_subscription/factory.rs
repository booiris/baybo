//! Factory for the openai-subscription provider.

use super::{
    OpenAiSubscriptionCompletionModel, PROVIDER_NAME, VaultTokenStore,
    completion_model::BackgroundRefresh,
};
use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, LlmError, ModelInfo, ModelPricing};

/// Hosts where the ChatGPT subscription bearer is allowed to land. Match
/// is suffix-based (`chatgpt.com` covers `api.chatgpt.com` etc.). A
/// non-default `base_url` outside the list is rejected at factory time
/// so a malicious aura.json can't exfiltrate the OAuth bearer to an
/// attacker host on the next chat call.
const ALLOWED_HOST_SUFFIXES: &[&str] = &["chatgpt.com", "auth.openai.com"];

/// Env-var escape hatch for non-OpenAI hosts. Env rather than aura.json
/// so flipping a credential-leak guard requires an explicit shell action.
pub const UNSAFE_BASE_URL_ENV_VAR: &str = "AURA_OPENAI_SUBSCRIPTION_UNSAFE_BASE_URL";

fn validate_base_url(base_url: Option<&str>) -> crate::Result<()> {
    let Some(url_str) = base_url else {
        return Ok(());
    };
    let parsed = url::Url::parse(url_str).map_err(|e| {
        LlmError::Config(format!(
            "openai-subscription: base_url is not a valid URL ({e}): {url_str}"
        ))
    })?;
    // Scheme check runs before the host check so `http://chatgpt.com`
    // (allowlisted host, plaintext transport) is rejected. The unsafe
    // override only widens the host allowlist, never the scheme.
    if parsed.scheme() != "https" {
        return Err(LlmError::Config(format!(
            "openai-subscription: base_url must use https:// (got scheme {:?} in {url_str:?}). \
             ChatGPT OAuth bearers must never traverse plaintext transport.",
            parsed.scheme(),
        )));
    }
    let host = parsed.host_str().ok_or_else(|| {
        LlmError::Config(format!(
            "openai-subscription: base_url has no host component: {url_str}"
        ))
    })?;
    if ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
    {
        return Ok(());
    }
    if std::env::var(UNSAFE_BASE_URL_ENV_VAR).is_ok() {
        tracing::warn!(
            event = "openai_subscription_unsafe_base_url",
            base_url = %url_str,
            host = %host,
            "openai-subscription: bearer routed to non-OpenAI host via {UNSAFE_BASE_URL_ENV_VAR}"
        );
        return Ok(());
    }
    Err(LlmError::Config(format!(
        "openai-subscription: refusing to send the ChatGPT OAuth bearer to host {host:?} \
         (base_url={url_str:?}). Allowed hosts: {ALLOWED_HOST_SUFFIXES:?}. To override \
         (you assume the TOS / credential-leak risk), set {UNSAFE_BASE_URL_ENV_VAR}=1 \
         in the env."
    )))
}

pub struct OpenAiSubscriptionProviderFactory;

#[async_trait::async_trait]
impl LlmProviderFactory for OpenAiSubscriptionProviderFactory {
    fn provider_name(&self) -> &str {
        PROVIDER_NAME
    }

    fn known_models(&self) -> &'static [&'static str] {
        // The Codex catalog is account-tier-dependent; static listing
        // would lie. Use `aura llm models --live --provider openai-subscription`
        // to see what the signed-in account actually has.
        &[]
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let model = build_model(config, BackgroundRefresh::Enabled)?;
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: PROVIDER_NAME.to_string(),
            context_window: 272_000,
            supports_tools: true,
            supports_vision: false,
            // Subscription billing is account-level, not per-token, so cost
            // records land at $0 — see design doc.
            pricing: ModelPricing {
                input_per_1m_tokens: 0.0,
                output_per_1m_tokens: 0.0,
            },
        };
        Ok(LlmClient::new(
            model_info,
            AnyCompletionModel::OpenAiSubscription(model),
        ))
    }

    async fn live_models(&self, config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
        // Throwaway model — `Disabled` so this one-shot probe doesn't
        // spawn a background-refresh task that outlives the call.
        let model = build_model(config, BackgroundRefresh::Disabled)?;
        model.list_remote_models().await
    }
}

fn build_model(
    config: &LlmProviderConfig,
    background: BackgroundRefresh,
) -> crate::Result<OpenAiSubscriptionCompletionModel> {
    let vault = config.vault.as_ref().ok_or_else(|| {
        LlmError::Config(
            "openai-subscription: SecretVault not provided. Boot wires it for normal runs; \
             argv-mode probes (e.g. `aura llm models`) cannot use this provider."
                .into(),
        )
    })?;
    validate_base_url(config.base_url.as_deref())?;
    let token_store = VaultTokenStore::new(vault.clone());
    Ok(OpenAiSubscriptionCompletionModel::new(
        config.model.clone(),
        config.base_url.clone(),
        config.reasoning_effort.as_deref(),
        token_store,
        background,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_security::{EncryptionKey, SecretVault};
    use aura_storage::test_support::MemorySecretStore;
    use std::sync::Arc;

    fn vault() -> Arc<SecretVault> {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())))
    }

    fn cfg(vault: Option<Arc<SecretVault>>) -> LlmProviderConfig {
        LlmProviderConfig {
            provider: PROVIDER_NAME.into(),
            api_key: None,
            base_url: None,
            model: "gpt-5".into(),
            supports_vision: None,
            reasoning_effort: None,
            vault,
        }
    }

    #[test]
    fn create_requires_vault() {
        let factory = OpenAiSubscriptionProviderFactory;
        // LlmClient is intentionally not Debug; expand unwrap_err manually.
        let err = match factory.create(&cfg(None)) {
            Ok(_) => panic!("expected Err when vault is None"),
            Err(e) => e,
        };
        match err {
            LlmError::Config(msg) => assert!(msg.contains("SecretVault not provided")),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_with_vault_succeeds() {
        // tokio runtime needed: production-path factory spawns the background
        // refresh task. Test does no other async work; the task goes idle
        // and is dropped at runtime teardown.
        let client = OpenAiSubscriptionProviderFactory
            .create(&cfg(Some(vault())))
            .expect("create should succeed");
        assert_eq!(client.model_info().provider, PROVIDER_NAME);
        assert_eq!(client.model_info().id, "gpt-5");
        assert_eq!(client.model_info().pricing.input_per_1m_tokens, 0.0);
    }

    // base_url validator regressions.

    #[test]
    fn validate_base_url_accepts_none() {
        assert!(validate_base_url(None).is_ok());
    }

    #[test]
    fn validate_base_url_accepts_default_chatgpt_host() {
        assert!(validate_base_url(Some("https://chatgpt.com/backend-api")).is_ok());
    }

    #[test]
    fn validate_base_url_accepts_chatgpt_subdomain() {
        assert!(validate_base_url(Some("https://api.chatgpt.com/backend-api")).is_ok());
    }

    #[test]
    fn validate_base_url_accepts_auth_openai_host() {
        assert!(validate_base_url(Some("https://auth.openai.com")).is_ok());
    }

    #[test]
    fn validate_base_url_rejects_attacker_host() {
        let err = validate_base_url(Some("https://attacker.example/backend-api"))
            .expect_err("must reject non-OpenAI hosts by default");
        match err {
            LlmError::Config(msg) => {
                assert!(msg.contains("attacker.example"));
                assert!(msg.contains(UNSAFE_BASE_URL_ENV_VAR));
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn validate_base_url_rejects_chatgpt_lookalike() {
        // Suffix check requires exact host match or leading dot — bare
        // ".com.attacker.example" suffix doesn't qualify as chatgpt.com.
        let err = validate_base_url(Some("https://chatgpt.com.attacker.example"))
            .expect_err("lookalike hosts must be rejected");
        assert!(matches!(err, LlmError::Config(_)));
    }

    #[test]
    fn validate_base_url_rejects_invalid_url() {
        let err =
            validate_base_url(Some("not even a url")).expect_err("malformed URL must be rejected");
        match err {
            LlmError::Config(msg) => assert!(msg.contains("not a valid URL")),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn validate_base_url_rejects_http_with_allowlisted_host() {
        let err = validate_base_url(Some("http://chatgpt.com/backend-api"))
            .expect_err("http:// must be rejected even on allowlisted host");
        match err {
            LlmError::Config(msg) => {
                assert!(msg.contains("https://"));
                assert!(msg.contains("plaintext"));
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn validate_base_url_rejects_http_with_allowlisted_subdomain() {
        let err = validate_base_url(Some("http://api.chatgpt.com/backend-api"))
            .expect_err("http:// on subdomain must also be rejected");
        assert!(matches!(err, LlmError::Config(_)));
    }

    #[test]
    fn validate_base_url_rejects_non_http_scheme() {
        let err = validate_base_url(Some("ftp://chatgpt.com/backend-api"))
            .expect_err("non-http(s) schemes must be rejected");
        assert!(matches!(err, LlmError::Config(_)));
    }

    #[test]
    fn validate_base_url_https_check_runs_before_host_check() {
        // Error must cite scheme/plaintext, not "host not allowlisted":
        // the scheme is the more specific failure and operators need to
        // see the actual reason.
        let err = validate_base_url(Some("http://attacker.example/backend-api"))
            .expect_err("non-https on non-allowlisted host should still cite scheme");
        match err {
            LlmError::Config(msg) => {
                assert!(
                    msg.contains("https://") || msg.contains("plaintext"),
                    "scheme check must fire first: {msg}"
                );
                assert!(
                    !msg.contains("Allowed hosts"),
                    "host allowlist message should not appear when the scheme is the problem: {msg}"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }
}
