use rig::client::CompletionClient;
use rig::providers::anthropic;

use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};

/// Factory that creates `LlmClient` instances configured for Anthropic Claude models.
pub struct AnthropicProviderFactory;

impl LlmProviderFactory for AnthropicProviderFactory {
    fn provider_name(&self) -> &str {
        "anthropic"
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or_else(|| crate::LlmError::Config("Anthropic requires an API key".into()))?;

        let client = match config.base_url {
            Some(ref base_url) => anthropic::Client::builder()
                .api_key(api_key)
                .base_url(base_url)
                .build(),
            None => anthropic::Client::new(api_key),
        }
        .map_err(|e| crate::LlmError::Config(format!("failed to create Anthropic client: {e}")))?;

        let model = client.completion_model(&config.model);

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

        Ok(LlmClient::new(
            model_info,
            AnyCompletionModel::Anthropic(model),
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
}
