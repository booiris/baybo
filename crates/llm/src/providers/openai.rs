use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{LlmClient, ModelInfo, ModelPricing, ResponseParseMode};

/// Factory that creates `LlmClient` instances configured for OpenAI models.
pub struct OpenAIProviderFactory;

impl LlmProviderFactory for OpenAIProviderFactory {
    fn provider_name(&self) -> &str {
        "openai"
    }

    fn create(&self, config: &LlmProviderConfig) -> aura_core::Result<LlmClient> {
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "openai".to_string(),
            context_window: 128_000,
            supports_tools: true,
            supports_vision: true,
            pricing: ModelPricing {
                input_per_1m_tokens: 2.50,
                output_per_1m_tokens: 10.0,
            },
        };
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

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
        let factory = OpenAIProviderFactory;
        assert_eq!(factory.provider_name(), "openai");
    }

    #[test]
    fn test_create_client() {
        let factory = OpenAIProviderFactory;
        let config = LlmProviderConfig {
            provider: "openai".to_string(),
            api_key: Some("sk-test".to_string()),
            base_url: None,
            model: "gpt-4o".to_string(),
        };
        let client = factory.create(&config).unwrap();
        assert_eq!(client.model_id(), "gpt-4o");
        assert_eq!(client.model_info().context_window, 128_000);
        assert!(client.model_info().supports_tools);
    }
}
