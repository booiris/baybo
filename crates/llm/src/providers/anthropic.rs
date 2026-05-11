use rig::client::CompletionClient;
use rig::providers::anthropic;
use serde::Deserialize;

use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};
use aura_model::MicroUsd;

pub(crate) const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Factory that creates `LlmClient` instances configured for Anthropic Claude models.
pub struct AnthropicProviderFactory;

#[async_trait::async_trait]
impl LlmProviderFactory for AnthropicProviderFactory {
    fn provider_name(&self) -> &str {
        "anthropic"
    }

    fn known_models(&self) -> &'static [&'static str] {
        &[
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
            "claude-opus-4",
            "claude-sonnet-4",
        ]
    }

    /// Sonnet flagship list price; over-attribution is the safe
    /// direction since the budget gate is the safety surface.
    fn flat_default_pricing(&self) -> ModelPricing {
        ModelPricing {
            input_per_1m_tokens: MicroUsd::from_usd_decimal(3.0),
            output_per_1m_tokens: MicroUsd::from_usd_decimal(15.0),
            ..Default::default()
        }
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

        // rig's Anthropic model ships with caching off; without this the
        // request omits cache_control breakpoints and Anthropic returns
        // cached_input_tokens=0 / cache_creation_input_tokens=0 on every
        // call. `with_prompt_caching` marks the system prompt + last
        // message as ephemeral, which also caches the (tools, system,
        // history) prefix on every subsequent turn.
        let model = client.completion_model(&config.model).with_prompt_caching();

        let caps = crate::openrouter::capabilities_for(self.provider_name(), &config.model);
        let defaults = crate::providers::factory_defaults_for(self.provider_name());
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "anthropic".to_string(),
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
            AnyCompletionModel::Anthropic(model),
        ))
    }

    async fn live_models(&self, config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
        let api_key = config.api_key.as_deref().ok_or_else(|| {
            crate::LlmError::Config("Anthropic live discovery requires an API key".into())
        })?;
        let base = config
            .base_url
            .as_deref()
            .unwrap_or(ANTHROPIC_DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}/v1/models");
        let resp = reqwest::Client::new()
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "anthropic GET /v1/models"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::status_to_error(
                status.as_u16(),
                format!("anthropic GET /v1/models returned {status}: {body}"),
            ));
        }
        let payload: ModelListResponse = resp.json().await.map_err(|e| {
            crate::LlmError::Decode(format!("anthropic /v1/models: parse response: {e}"))
        })?;
        Ok(payload
            .data
            .into_iter()
            .map(|m| {
                let extras = serde_json::json!({
                    "type": m.kind,
                    "created_at": m.created_at,
                });
                LiveModelInfo {
                    id: m.id,
                    display_name: m.display_name,
                    extras,
                    ..Default::default()
                }
            })
            .collect())
    }
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
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
