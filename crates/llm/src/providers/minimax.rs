use rig::client::CompletionClient;
use rig::providers::anthropic;
use serde::Deserialize;

use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};
use aura_model::MicroUsd;

pub(crate) const MINIMAX_DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/anthropic";
/// Origin host MiniMax exposes for OpenAI-compatible model listing.
/// Operators on the international cluster (`api.minimax.io`) can flip
/// this via the `MINIMAX_MODELS_BASE` env var.
const MINIMAX_DEFAULT_MODELS_BASE: &str = "https://api.minimaxi.com/v1";

/// Factory that creates `LlmClient` instances configured for MiniMax models.
///
/// MiniMax exposes an Anthropic-compatible Messages API, so we route
/// through rig's Anthropic client with the MiniMax base URL pinned by
/// default. Operators can override `base_url` (e.g. the international
/// `https://api.minimax.io/anthropic` endpoint) via `LlmConfig.base_url`.
pub struct MiniMaxProviderFactory;

#[async_trait::async_trait]
impl LlmProviderFactory for MiniMaxProviderFactory {
    fn provider_name(&self) -> &str {
        "minimax"
    }

    fn known_models(&self) -> &'static [&'static str] {
        &[
            "MiniMax-M2",
            "MiniMax-M1",
            "MiniMax-Text-01",
            "abab6.5s-chat",
            "abab6.5-chat",
        ]
    }

    /// Roughly M1's list price for unmapped ids (`abab*-chat`,
    /// custom checkpoints): M2 is cheaper, abab is unpriced on
    /// OpenRouter — M1's rate is the conservative midline.
    fn flat_default_pricing(&self) -> ModelPricing {
        ModelPricing {
            input_per_1m_tokens: MicroUsd::from_usd_decimal(0.40),
            output_per_1m_tokens: MicroUsd::from_usd_decimal(2.20),
            ..Default::default()
        }
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or_else(|| crate::LlmError::Config("MiniMax requires an API key".into()))?;

        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or(MINIMAX_DEFAULT_BASE_URL);

        let client = anthropic::Client::builder()
            .api_key(api_key)
            .base_url(base_url)
            .build()
            .map_err(|e| {
                crate::LlmError::Config(format!("failed to create MiniMax client: {e}"))
            })?;

        let model = client.completion_model(&config.model);

        let caps = crate::openrouter::capabilities_for(self.provider_name(), &config.model);
        let defaults = crate::providers::factory_defaults_for(self.provider_name());
        let model_info = ModelInfo {
            id: config.model.clone(),
            provider: "minimax".to_string(),
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
            crate::LlmError::Config("MiniMax live discovery requires an API key".into())
        })?;
        // The configured `base_url` points at MiniMax's
        // Anthropic-compat surface (`/anthropic`). Their model list
        // lives on the OpenAI-compat surface (`/v1/models`); derive
        // it by swapping the trailing path component on the same
        // host. Operators on a custom host who need to override can
        // set `MINIMAX_MODELS_BASE` in the env.
        let base = std::env::var("MINIMAX_MODELS_BASE")
            .unwrap_or_else(|_| derive_models_base(config.base_url.as_deref()));
        let url = format!("{}/models", base.trim_end_matches('/'));
        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "minimax GET /v1/models"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::status_to_error(
                status.as_u16(),
                format!("minimax GET /v1/models returned {status}: {body}"),
            ));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "minimax GET /v1/models read body"))?;
        let payload: ModelListResponse = serde_json::from_str(&body).map_err(|e| {
            crate::LlmError::Decode(format!(
                "minimax /v1/models: parse response: {e}; body: {}",
                truncate_body(&body)
            ))
        })?;
        let mut out: Vec<LiveModelInfo> = payload
            .data
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                let id = m.id.or(m.model)?;
                Some(LiveModelInfo {
                    id,
                    ..Default::default()
                })
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

const MINIMAX_BODY_TRUNCATE: usize = 512;

fn truncate_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= MINIMAX_BODY_TRUNCATE {
        trimmed.to_string()
    } else {
        let mut end = MINIMAX_BODY_TRUNCATE;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

fn derive_models_base(chat_base: Option<&str>) -> String {
    // `https://api.minimax.io/anthropic` → `https://api.minimax.io/v1`
    // Falls back to the global default when no custom host is set or
    // the input doesn't end in `/anthropic`.
    match chat_base {
        Some(url) => match url.trim_end_matches('/').strip_suffix("/anthropic") {
            Some(host) => format!("{host}/v1"),
            None => MINIMAX_DEFAULT_MODELS_BASE.to_string(),
        },
        None => MINIMAX_DEFAULT_MODELS_BASE.to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    // MiniMax returns `data: null` when the catalog is empty — accept that
    // shape (and a missing field) without erroring on the deserialize step.
    #[serde(default)]
    data: Option<Vec<ModelEntry>>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let factory = MiniMaxProviderFactory;
        assert_eq!(factory.provider_name(), "minimax");
    }

    #[test]
    fn truncate_body_preserves_short_input() {
        assert_eq!(truncate_body("short body"), "short body");
        assert_eq!(truncate_body("  trimmed  "), "trimmed");
    }

    #[test]
    fn truncate_body_caps_long_input_with_ellipsis() {
        let long = "x".repeat(MINIMAX_BODY_TRUNCATE + 50);
        let truncated = truncate_body(&long);
        assert!(truncated.ends_with('…'));
        // ASCII body, so truncation lands on a char boundary at exactly the cap.
        assert_eq!(
            truncated.chars().filter(|c| *c == 'x').count(),
            MINIMAX_BODY_TRUNCATE
        );
    }

    #[test]
    fn model_entry_accepts_id_or_model_field() {
        // Standard OpenAI-compat shape: `id`.
        let parsed: ModelListResponse =
            serde_json::from_str(r#"{"data":[{"id":"abab6.5s-chat"}]}"#).unwrap();
        let data = parsed.data.unwrap();
        assert_eq!(data[0].id.as_deref(), Some("abab6.5s-chat"));

        // MiniMax variant where the entry uses `model` instead of `id`.
        let parsed: ModelListResponse =
            serde_json::from_str(r#"{"data":[{"model":"MiniMax-M2"}]}"#).unwrap();
        let data = parsed.data.unwrap();
        assert_eq!(data[0].model.as_deref(), Some("MiniMax-M2"));
        assert!(data[0].id.is_none());

        // MiniMax China cluster's actual response: `data: null`.
        let parsed: ModelListResponse =
            serde_json::from_str(r#"{"object":"","data":null}"#).unwrap();
        assert!(parsed.data.is_none());

        // Missing field also parses.
        let parsed: ModelListResponse = serde_json::from_str(r#"{}"#).unwrap();
        assert!(parsed.data.is_none());
    }

    #[test]
    fn test_create_requires_api_key() {
        let factory = MiniMaxProviderFactory;
        let config = LlmProviderConfig {
            provider: "minimax".into(),
            api_key: None,
            base_url: None,
            model: "MiniMax-M2".into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
        };
        assert!(factory.create(&config).is_err());
    }

    #[test]
    fn test_create_with_api_key_uses_default_base_url() {
        let factory = MiniMaxProviderFactory;
        let config = LlmProviderConfig {
            provider: "minimax".into(),
            api_key: Some("test-key".into()),
            base_url: None,
            model: "MiniMax-M2".into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
        };
        let client = factory.create(&config).expect("client builds with api key");
        assert_eq!(client.model_info().provider, "minimax");
        assert_eq!(client.model_info().id, "MiniMax-M2");
    }

    #[test]
    fn test_known_models_lists_flagship() {
        let factory = MiniMaxProviderFactory;
        assert!(factory.known_models().contains(&"MiniMax-M2"));
    }

    #[test]
    fn factory_default_keeps_vision_off_for_text_first_models() {
        // M2 generation is text-first; the factory must NOT claim
        // vision by default, otherwise the runtime base64-encodes a
        // 100 MiB image into a request that gets silently flattened
        // to a URL on the server side.
        let factory = MiniMaxProviderFactory;
        let config = LlmProviderConfig {
            provider: "minimax".into(),
            api_key: Some("test-key".into()),
            base_url: None,
            model: "MiniMax-M2".into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
        };
        let client = factory.create(&config).unwrap();
        assert!(!client.model_info().supports_vision);
    }

    #[test]
    fn registry_supports_vision_override_flips_the_flag() {
        // Operator on a truly multimodal MiniMax model (e.g. VL-01)
        // can opt in via config without recompiling. This is the
        // post-factory override path inside `create_client`.
        let registry = crate::registry::LlmProviderRegistry::with_default_providers();
        let client = registry
            .build_client(&LlmProviderConfig {
                provider: "minimax".into(),
                api_key: Some("test-key".into()),
                base_url: None,
                model: "MiniMax-VL-01".into(),
                supports_vision: Some(true),
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
            })
            .unwrap();
        assert!(client.model_info().supports_vision);
    }

    #[test]
    fn registry_supports_vision_override_can_force_off() {
        // Symmetric path: a provider that claims vision can be
        // explicitly disabled (e.g. local-only deployments where the
        // image path costs are unwanted).
        let registry = crate::registry::LlmProviderRegistry::with_default_providers();
        let client = registry
            .build_client(&LlmProviderConfig {
                provider: "anthropic".into(),
                api_key: Some("test-key".into()),
                base_url: None,
                model: "claude-sonnet-4-6".into(),
                supports_vision: Some(false),
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
            })
            .unwrap();
        assert!(!client.model_info().supports_vision);
    }
}
