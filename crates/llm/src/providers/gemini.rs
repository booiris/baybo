use rig::client::CompletionClient;
use serde::Deserialize;

use crate::providers::rig_providers::rig_provider_factory;
use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelPricing};

// Host root only — rig's gemini client appends the versioned path
// (`/v1beta/models/{model}:generateContent`) on every request, so the
// base URL must NOT include `/v1beta` or we end up with `/v1beta/v1beta/…`.
pub(crate) const GEMINI_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

rig_provider_factory!(
    GeminiProviderFactory,
    "gemini",
    gemini,
    Gemini,
    priced = crate::providers::catalog::GEMINI_FLAT_DEFAULT_PRICING,
    live_models = gemini_live_models,
    api_key_env = "GEMINI_API_KEY",
    base_url = GEMINI_DEFAULT_BASE_URL
);

/// Live discovery against Gemini's catalog endpoint. Filters to models
/// that can `generateContent` (skipping embedding/tuning-only entries)
/// and strips the API's `models/` id prefix so the slug matches the
/// shape `CompletionModel` expects.
async fn gemini_live_models(config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
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
    // Gemini API keys are alphanumeric — no encoding needed — but we
    // still send the key as a header where supported, so it doesn't
    // land in URL access logs. Header form is documented at
    // <https://ai.google.dev/gemini-api/docs/api-key>.
    let url = format!("{host_root}/v1beta/models?pageSize=1000");
    let resp = crate::proxied_client(config.proxy.as_ref())?
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
