use rig::client::CompletionClient;
use rig::providers::gemini;

use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};

/// Factory that creates `LlmClient` instances configured for Google Gemini models.
pub struct GeminiProviderFactory;

impl LlmProviderFactory for GeminiProviderFactory {
    fn provider_name(&self) -> &str {
        "gemini"
    }

    fn known_models(&self) -> &'static [&'static str] {
        &[
            "gemini-3.1-pro-preview",
            "gemini-3-flash-preview",
            "gemini-3.1-flash-lite-preview",
            "gemini-2.5-flash",
            "gemini-2.5-pro",
        ]
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or_else(|| crate::LlmError::Config("Gemini requires an API key".into()))?;

        let client = match config.base_url {
            Some(ref base_url) => gemini::Client::builder()
                .api_key(api_key)
                .base_url(base_url)
                .build(),
            None => gemini::Client::new(api_key),
        }
        .map_err(|e| crate::LlmError::Config(format!("failed to create Gemini client: {e}")))?;

        let model = client.completion_model(&config.model);

        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "gemini".to_string(),
            context_window: 1_000_000,
            supports_tools: true,
            supports_vision: true,
            pricing: ModelPricing {
                input_per_1m_tokens: 0.075,
                output_per_1m_tokens: 0.30,
            },
        };

        Ok(LlmClient::new(
            model_info,
            AnyCompletionModel::Gemini(model),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let factory = GeminiProviderFactory;
        assert_eq!(factory.provider_name(), "gemini");
    }
}
