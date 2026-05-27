use rig::client::CompletionClient;
use rig::providers::openai;

use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};

/// DeepSeek's OpenAI-compatible base. The platform speaks the OpenAI
/// Chat Completions wire format (function calling + streaming
/// included), so we route through rig's OpenAI client with this base
/// URL pinned. Operators can override via `LlmConfig.base_url`.
pub(crate) const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// Factory that creates `LlmClient` instances configured for DeepSeek models.
///
/// DeepSeek exposes an OpenAI-compatible Chat Completions API, so this
/// mirrors `OpenAIProviderFactory` but pins the DeepSeek base URL by
/// default and dispatches through [`AnyCompletionModel::OpenAI`].
pub struct DeepSeekProviderFactory;

#[async_trait::async_trait]
impl LlmProviderFactory for DeepSeekProviderFactory {
    fn provider_name(&self) -> &str {
        "deepseek"
    }

    /// Priciest OpenRouter `deepseek/*` model by input+output, the
    /// unknown-id fallback. Erring toward the higher rate keeps the
    /// budget gate conservative (under-attribution is the unsafe
    /// direction). Generated from the snapshot — see `build.rs`.
    fn flat_default_pricing(&self) -> ModelPricing {
        crate::providers::catalog::DEEPSEEK_FLAT_DEFAULT_PRICING
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or_else(|| crate::LlmError::Config("DeepSeek requires an API key".into()))?;

        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or(DEEPSEEK_DEFAULT_BASE_URL);

        let client = openai::Client::builder()
            .api_key(api_key)
            .base_url(base_url)
            .http_client(crate::proxied_client(config.proxy.as_ref())?)
            .build()
            .map_err(|e| {
                crate::LlmError::Config(format!("failed to create DeepSeek client: {e}"))
            })?;

        let model = client.completions_api().completion_model(&config.model);

        let caps = crate::openrouter::capabilities_for(self.provider_name(), &config.model);
        let defaults = crate::providers::factory_defaults_for(self.provider_name());
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "deepseek".to_string(),
            context_window: caps
                .and_then(|c| c.context_window)
                .unwrap_or(defaults.context_window),
            supports_tools: true,
            supports_vision: caps
                .and_then(|c| c.supports_vision)
                .unwrap_or(defaults.supports_vision),
            pricing: self.pricing_for_model(&config.model),
        };

        Ok(LlmClient::new(
            model_info,
            AnyCompletionModel::OpenAI(model),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::LlmProviderConfig;
    use aura_model::MicroUsd;

    fn config(model: &str, api_key: Option<&str>) -> LlmProviderConfig {
        LlmProviderConfig {
            provider: "deepseek".into(),
            api_key: api_key.map(str::to_string),
            base_url: None,
            model: model.into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
            proxy: None,
        }
    }

    #[test]
    fn provider_name_is_deepseek() {
        assert_eq!(DeepSeekProviderFactory.provider_name(), "deepseek");
    }

    #[test]
    fn create_requires_api_key() {
        assert!(
            DeepSeekProviderFactory
                .create(&config("deepseek-chat", None))
                .is_err()
        );
    }

    #[test]
    fn create_with_api_key_pins_provider_and_model() {
        let client = DeepSeekProviderFactory
            .create(&config("deepseek-chat", Some("test-key")))
            .expect("client builds with api key");
        assert_eq!(client.model_info().provider, "deepseek");
        assert_eq!(client.model_info().id, "deepseek-chat");
    }

    #[test]
    fn unknown_model_falls_back_to_nonzero_flat_pricing() {
        // The flat default must stay non-zero so a snapshot miss never
        // silently disables the budget gate — it's what seeds the cost
        // lookup for a configured id the snapshot doesn't price.
        let p = DeepSeekProviderFactory.pricing_for_model("deepseek-custom-checkpoint");
        assert!(p.input_per_1m_tokens > MicroUsd::ZERO);
        assert!(p.output_per_1m_tokens > MicroUsd::ZERO);
    }
}
