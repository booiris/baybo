use rig::client::CompletionClient;
use rig::providers::openai;
use serde::Deserialize;

use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};
use aura_model::MicroUsd;

pub(crate) const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Factory that creates `LlmClient` instances configured for OpenAI models.
pub struct OpenAIProviderFactory;

#[async_trait::async_trait]
impl LlmProviderFactory for OpenAIProviderFactory {
    fn provider_name(&self) -> &str {
        "openai"
    }

    fn known_models(&self) -> &'static [&'static str] {
        &[
            "gpt-5",
            "gpt-5-mini",
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4o",
            "gpt-4o-mini",
        ]
    }

    /// Flagship list price; under-attribution is worse than
    /// over-attribution since the budget gate is the safety surface.
    fn flat_default_pricing(&self) -> ModelPricing {
        ModelPricing {
            input_per_1m_tokens: MicroUsd::from_usd_decimal(2.50),
            output_per_1m_tokens: MicroUsd::from_usd_decimal(10.0),
            ..Default::default()
        }
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
            pricing: self.pricing_for_model(&config.model),
        };

        Ok(LlmClient::new(
            model_info,
            AnyCompletionModel::OpenAI(model),
        ))
    }

    async fn live_models(&self, config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
        let api_key = config.api_key.as_deref().ok_or_else(|| {
            crate::LlmError::Config("OpenAI live discovery requires an API key".into())
        })?;
        let base = config
            .base_url
            .as_deref()
            .unwrap_or(OPENAI_DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}/models");
        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "openai GET /models"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::status_to_error(
                status.as_u16(),
                format!("openai GET /models returned {status}: {body}"),
            ));
        }
        let payload: ModelListResponse = resp
            .json()
            .await
            .map_err(|e| crate::LlmError::Decode(format!("openai /models: parse response: {e}")))?;
        let mut out: Vec<LiveModelInfo> = payload
            .data
            .into_iter()
            .map(|m| {
                let extras = serde_json::json!({
                    "owned_by": m.owned_by,
                    "created": m.created,
                });
                LiveModelInfo {
                    id: m.id,
                    extras,
                    ..Default::default()
                }
            })
            .collect();
        // Stable order so the picker doesn't shuffle on each call.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
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
    owned_by: Option<String>,
    #[serde(default)]
    created: Option<i64>,
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
