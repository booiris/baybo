use serde::{Deserialize, Serialize};

/// LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LlmConfig {
    /// Provider identifier, e.g. `"openai"`, `"anthropic"`, `"gemini"`, or `"minimax"`.
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
    /// Override the provider's default `supports_vision` flag.
    ///
    /// When `Some(true)`, the LLM client encodes inbound `Image` /
    /// `Audio` / `File` content blocks as proper multimodal parts;
    /// when `Some(false)`, they degrade to a `[image: …]` text stub
    /// even on a vision-capable model. `None` keeps the factory
    /// default.
    ///
    /// Why this is overridable: providers don't always behave like
    /// their multimodal flag suggests. MiniMax-M2 advertises an
    /// OpenAI-compatible API but silently uploads any inline image
    /// to its OSS and shows the model only the URL — the conversion
    /// succeeds, the model can't actually see the picture, and
    /// nothing surfaces an error. For these "advertises support but
    /// doesn't really" cases, an operator needs to flip vision off
    /// for that specific deployment without recompiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            fallback_model: None,
            api_key_env: None,
            base_url: None,
            supports_vision: None,
        }
    }
}
