use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{LlmClient, ModelInfo, ModelPricing, ResponseParseMode};

/// Factory that creates `LlmClient` instances configured for Anthropic Claude models.
pub struct AnthropicProviderFactory;

impl LlmProviderFactory for AnthropicProviderFactory {
    fn provider_name(&self) -> &str {
        "anthropic"
    }

    fn create(&self, config: &LlmProviderConfig) -> aura_core::Result<LlmClient> {
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "anthropic".to_string(),
            context_window: 200_000,
            supports_tools: true,
            supports_vision: true,
            pricing: ModelPricing {
                input_per_1m_tokens: 3.0,
                output_per_1m_tokens: 15.0,
            },
        };
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());

        Ok(LlmClient::new(
            model_info,
            ResponseParseMode::NativeFunctionCalling,
            config.api_key.clone(),
            base_url,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let factory = AnthropicProviderFactory;
        assert_eq!(factory.provider_name(), "anthropic");
    }

    #[test]
    fn test_create_client() {
        let factory = AnthropicProviderFactory;
        let config = LlmProviderConfig {
            provider: "anthropic".to_string(),
            api_key: Some("sk-ant-test".to_string()),
            base_url: None,
            model: "claude-sonnet-4-6".to_string(),
        };
        let client = factory.create(&config).unwrap();
        assert_eq!(client.model_id(), "claude-sonnet-4-6");
        assert_eq!(client.model_info().context_window, 200_000);
        assert!(client.model_info().supports_tools);
    }
}
