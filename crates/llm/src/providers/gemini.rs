use rig::client::CompletionClient;
use rig::providers::gemini;
use serde::Deserialize;

use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};
use aura_model::MicroUsd;

// Host root only — rig's gemini client appends the versioned path
// (`/v1beta/models/{model}:generateContent`) on every request, so the
// base URL must NOT include `/v1beta` or we end up with `/v1beta/v1beta/…`.
pub(crate) const GEMINI_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Factory that creates `LlmClient` instances configured for Google Gemini models.
pub struct GeminiProviderFactory;

#[async_trait::async_trait]
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

    /// Gemini 2.5 Flash list price; over-attribution is the safe
    /// direction since the budget gate is the safety surface.
    fn flat_default_pricing(&self) -> ModelPricing {
        ModelPricing {
            input_per_1m_tokens: MicroUsd::from_usd_decimal(0.075),
            output_per_1m_tokens: MicroUsd::from_usd_decimal(0.30),
        }
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
            pricing: self.pricing_for_model(&config.model),
        };

        Ok(LlmClient::new(
            model_info,
            AnyCompletionModel::Gemini(model),
        ))
    }

    async fn live_models(&self, config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
        let api_key = config.api_key.as_deref().ok_or_else(|| {
            crate::LlmError::Config("Gemini live discovery requires an API key".into())
        })?;
        let base = config
            .base_url
            .as_deref()
            .unwrap_or(GEMINI_DEFAULT_BASE_URL)
            .trim_end_matches('/');
        // Tolerate stale configs whose base already includes the version
        // segment — the canonical base is the host root.
        let host_root = base.strip_suffix("/v1beta").unwrap_or(base);
        // Gemini API keys are alphanumeric — no encoding needed —
        // but we still send the key as a header where supported, so
        // it doesn't land in URL access logs. The header form is
        // documented at <https://ai.google.dev/gemini-api/docs/api-key>.
        let url = format!("{host_root}/v1beta/models?pageSize=1000");
        let resp = reqwest::Client::new()
            .get(&url)
            .header("x-goog-api-key", api_key)
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "gemini GET /models"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::status_to_error(
                status.as_u16(),
                format!("gemini GET /models returned {status}: {body}"),
            ));
        }
        let payload: ModelListResponse = resp
            .json()
            .await
            .map_err(|e| crate::LlmError::Decode(format!("gemini /models: parse response: {e}")))?;
        Ok(payload
            .models
            .into_iter()
            .filter(|m| {
                // Only surface models that can actually generate
                // content; skip embedding / tuning-only entries the
                // catalog returns alongside.
                m.supported_generation_methods
                    .iter()
                    .flatten()
                    .any(|s| s == "generateContent")
            })
            .map(|m| {
                // The API returns ids prefixed with "models/"; the
                // CompletionModel constructor expects the bare slug.
                let id = m
                    .name
                    .strip_prefix("models/")
                    .unwrap_or(&m.name)
                    .to_string();
                let extras = serde_json::json!({
                    "version": m.version,
                    "supported_generation_methods": m.supported_generation_methods,
                });
                LiveModelInfo {
                    id,
                    display_name: m.display_name,
                    description: m.description,
                    context_window: m.input_token_limit.map(|n| n as usize),
                    extras,
                    ..Default::default()
                }
            })
            .collect())
    }
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    #[serde(default)]
    models: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default, rename = "inputTokenLimit")]
    input_token_limit: Option<i64>,
    #[serde(default, rename = "supportedGenerationMethods")]
    supported_generation_methods: Option<Vec<String>>,
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
