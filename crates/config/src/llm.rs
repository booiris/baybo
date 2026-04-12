use serde::{Deserialize, Serialize};

/// LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LlmConfig {
    /// Provider identifier, e.g. `"openai"` or `"anthropic"`.
    pub provider: String,
    /// Primary model identifier, e.g. `"gpt-4o-mini"`.
    pub model: String,
    /// Optional fallback model if the primary call fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// Name of an environment variable holding the API key. When `None`, the
    /// consumer falls back to provider-specific defaults (e.g. `OPENAI_API_KEY`).
    /// The config never holds a literal API key — this field is a **reference**.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Custom base URL for the provider API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            fallback_model: None,
            api_key_env: None,
            base_url: None,
        }
    }
}
