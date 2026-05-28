use rig::client::CompletionClient;
use rig::providers::openai;
use serde::Deserialize;

use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};

pub(crate) const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Factory that creates `LlmClient` instances configured for OpenAI models.
pub struct OpenAIProviderFactory;

#[async_trait::async_trait]
impl LlmProviderFactory for OpenAIProviderFactory {
    fn provider_name(&self) -> &str {
        "openai"
    }

    /// Priciest OpenRouter `openai/*` model by input+output; over-attribution
    /// is the safe direction since the budget gate is the safety surface.
    /// Generated from the snapshot — see `build.rs`.
    fn flat_default_pricing(&self) -> ModelPricing {
        crate::providers::catalog::OPENAI_FLAT_DEFAULT_PRICING
    }

    fn default_base_url(&self) -> Option<&'static str> {
        Some(OPENAI_DEFAULT_BASE_URL)
    }

    fn default_api_key_env(&self) -> Option<&'static str> {
        Some("OPENAI_API_KEY")
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or_else(|| crate::LlmError::Config("OpenAI requires an API key".into()))?;

        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or(OPENAI_DEFAULT_BASE_URL);
        let client = openai::Client::builder()
            .api_key(api_key)
            .base_url(base_url)
            .http_client(crate::proxied_client(config.proxy.as_ref())?)
            .build()
            .map_err(|e| crate::LlmError::Config(format!("failed to create OpenAI client: {e}")))?;

        let model = client.completions_api().completion_model(&config.model);

        let caps = crate::openrouter::capabilities_for(self.provider_name(), &config.model);
        let defaults = crate::providers::factory_defaults_for(self.provider_name());
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "openai".to_string(),
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
        let resp = crate::proxied_client(config.proxy.as_ref())?
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
