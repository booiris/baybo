use rig::client::CompletionClient;
use rig::providers::openai;

use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};

const MINIMAX_DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/v1";

/// Factory that creates `LlmClient` instances configured for MiniMax models.
///
/// MiniMax exposes an OpenAI-compatible chat completions API, so we route
/// through rig's OpenAI client with the MiniMax base URL pinned by default.
/// Operators can override `base_url` (e.g. `https://api.minimaxi.com/v1` for
/// the China endpoint) via `LlmConfig.base_url`.
pub struct MiniMaxProviderFactory;

impl LlmProviderFactory for MiniMaxProviderFactory {
    fn provider_name(&self) -> &str {
        "minimax"
    }

    fn known_models(&self) -> &'static [&'static str] {
        &[
            "MiniMax-M2",
            "MiniMax-M1",
            "MiniMax-Text-01",
            "abab6.5s-chat",
            "abab6.5-chat",
        ]
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or_else(|| crate::LlmError::Config("MiniMax requires an API key".into()))?;

        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or(MINIMAX_DEFAULT_BASE_URL);

        let client = openai::Client::builder()
            .api_key(api_key)
            .base_url(base_url)
            .build()
            .map_err(|e| {
                crate::LlmError::Config(format!("failed to create MiniMax client: {e}"))
            })?;

        let model = client.completions_api().completion_model(&config.model);

        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "minimax".to_string(),
            context_window: 200_000,
            supports_tools: true,
            supports_vision: false,
            pricing: ModelPricing {
                input_per_1m_tokens: 0.30,
                output_per_1m_tokens: 1.20,
            },
        };

        Ok(
            LlmClient::new(model_info, AnyCompletionModel::OpenAI(model))
                .with_parse_inline_think_tags(true),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let factory = MiniMaxProviderFactory;
        assert_eq!(factory.provider_name(), "minimax");
    }

    #[test]
    fn test_create_requires_api_key() {
        let factory = MiniMaxProviderFactory;
        let config = LlmProviderConfig {
            provider: "minimax".into(),
            api_key: None,
            base_url: None,
            model: "MiniMax-M2".into(),
        };
        assert!(factory.create(&config).is_err());
    }

    #[test]
    fn test_create_with_api_key_uses_default_base_url() {
        let factory = MiniMaxProviderFactory;
        let config = LlmProviderConfig {
            provider: "minimax".into(),
            api_key: Some("test-key".into()),
            base_url: None,
            model: "MiniMax-M2".into(),
        };
        let client = factory.create(&config).expect("client builds with api key");
        assert_eq!(client.model_info().provider, "minimax");
        assert_eq!(client.model_info().id, "MiniMax-M2");
    }

    #[test]
    fn test_known_models_lists_flagship() {
        let factory = MiniMaxProviderFactory;
        assert!(factory.known_models().contains(&"MiniMax-M2"));
    }
}
