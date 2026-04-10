use rig::client::CompletionClient;
use rig::providers::openai;

use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};

/// Factory that creates `LlmClient` instances configured for OpenAI models.
pub struct OpenAIProviderFactory;

impl LlmProviderFactory for OpenAIProviderFactory {
    fn provider_name(&self) -> &str {
        "openai"
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or_else(|| crate::LlmError::Config("OpenAI requires an API key".into()))?;

        let client = match config.base_url {
            Some(ref base_url) => openai::Client::builder()
                .api_key(api_key)
                .base_url(base_url)
                .build(),
            None => openai::Client::new(api_key),
        }
        .map_err(|e| crate::LlmError::Config(format!("failed to create OpenAI client: {e}")))?;

        let model = client.completions_api().completion_model(&config.model);

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

        Ok(LlmClient::new(model_info, AnyCompletionModel::OpenAI(model)))
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
}
