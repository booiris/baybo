use aura_model::LlmEntryName;
pub use aura_model::LlmPricingOverride;
use serde::{Deserialize, Serialize};

/// One entry in the `llm` registry. Each entry is keyed by `name` and
/// describes a provider/model pair plus its credentials. Multiple
/// entries can target the same provider (e.g. one `openai`-based
/// "gpt-5" entry and another for "gpt-4o").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmEntry {
    /// Stable identifier for this entry. Referenced by `default-llm`
    /// and by the operator CLI. Must be unique within the `llm` list.
    pub name: LlmEntryName,
    /// Provider id, e.g. `"openai"`, `"anthropic"`, `"gemini"`,
    /// `"minimax"`, `"openai-subscription"`.
    pub provider: String,
    /// Model id, e.g. `"gpt-4o"`.
    pub model: String,
    /// Name of an environment variable holding the API key. The config
    /// never holds a literal API key — this field is a **reference**.
    /// `None` means "look up the per-entry vault key, then fall back to
    /// the provider-specific default env var" (e.g. `OPENAI_API_KEY`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Custom base URL for the provider API. `None` lets each provider
    /// pick its own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Operator override for the factory's default `supports_vision`.
    /// `None` keeps the factory default; `Some` forces the flag.
    ///
    /// Why this is overridable: providers don't always behave like
    /// their multimodal flag suggests. MiniMax-M2 advertises an
    /// OpenAI-compatible API but silently uploads any inline image to
    /// its OSS and shows the model only the URL — the conversion
    /// succeeds, the model can't actually see the picture, and nothing
    /// surfaces an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    /// Operator override for the factory's default `context_window`
    /// (max input + output tokens). `None` keeps the factory default
    /// (which itself prefers OpenRouter snapshot capabilities, then
    /// falls back to a per-provider constant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Operator override for per-model pricing. Each field is
    /// independently optional — unset fields keep the factory default
    /// (OpenRouter snapshot, or a flat per-provider rate when the
    /// snapshot doesn't cover the slug).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<LlmPricingOverride>,
    /// Reasoning effort for providers that expose it (currently only
    /// `openai-subscription` Codex Responses). One of `none`,
    /// `minimal`, `low`, `medium`, `high`, `xhigh`. The provider
    /// silently clamps to whatever the chosen `model` supports.
    /// `None` lets the provider pick a sensible default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}
