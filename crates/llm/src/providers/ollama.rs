use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::rig_adapter::build_tool_schema_prompt;
use crate::{LlmClient, ModelInfo, ModelPricing, ResponseParseMode};

/// Factory that creates `LlmClient` instances configured for local Ollama models.
///
/// Ollama models typically do not support native function calling, so the client
/// is configured with `ResponseParseMode::PromptGuided` to extract tool calls
/// from free-text responses.
pub struct OllamaProviderFactory;

impl LlmProviderFactory for OllamaProviderFactory {
    fn provider_name(&self) -> &str {
        "ollama"
    }

    fn create(&self, config: &LlmProviderConfig) -> aura_core::Result<LlmClient> {
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "ollama".to_string(),
            context_window: 8_192,
            supports_tools: false,
            supports_vision: false,
            pricing: ModelPricing {
                input_per_1m_tokens: 0.0,
                output_per_1m_tokens: 0.0,
            },
        };

        // Build an empty tool schema prompt; actual tools are injected at request
        // time when the agent prepares the ChatRequest.
        let tool_schema_prompt = build_tool_schema_prompt(&[]);

        let base_url = todo!();

        Ok(LlmClient::new(
            model_info,
            ResponseParseMode::PromptGuided { tool_schema_prompt },
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
        let factory = OllamaProviderFactory;
        assert_eq!(factory.provider_name(), "ollama");
    }

    #[test]
    fn test_create_client() {
        let factory = OllamaProviderFactory;
        let config = LlmProviderConfig {
            provider: "ollama".to_string(),
            api_key: None,
            base_url: Some("http://localhost:11434".to_string()),
            model: "llama3".to_string(),
        };
        let client = factory.create(&config).unwrap();
        assert_eq!(client.model_id(), "llama3");
        assert_eq!(client.model_info().context_window, 8_192);
        assert!(!client.model_info().supports_tools);
        assert_eq!(client.model_info().pricing.input_per_1m_tokens, 0.0);
    }
}
