use baybo_model::LlmEntryName;
pub use baybo_model::LlmPricingOverride;
use serde::{Deserialize, Serialize};

/// One model an [`LlmEntry`] can serve, plus the operator's overrides for
/// **that model's** facts.
///
/// Every field but `model` is a fact *about the model*, so it belongs next
/// to the model id rather than on the entry: one entry serves many models,
/// and the entry's provider/credentials are all they genuinely share.
/// Unset fields fall through to the provider factory's own per-model
/// resolution (the bundled OpenRouter snapshot, keyed by model slug,
/// then a per-provider constant).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmModelSpec {
    /// Model id as the provider names it, e.g. `"gpt-5"`.
    pub model: String,
    /// Operator override for `ModelInfo.context_window` (max input +
    /// output tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Operator override for per-token pricing. Each field is
    /// independently optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<LlmPricingOverride>,
    /// Operator override for the factory's default `supports_vision`.
    ///
    /// Why this is overridable: providers don't always behave like their
    /// multimodal flag suggests. MiniMax-M2 advertises an
    /// OpenAI-compatible API but silently uploads any inline image to its
    /// OSS and shows the model only the URL — the conversion succeeds,
    /// the model can't actually see the picture, and nothing surfaces an
    /// error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
}

impl LlmModelSpec {
    /// A model with no overrides.
    pub fn bare(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            context_window: None,
            pricing: None,
            supports_vision: None,
        }
    }
}

/// One entry in the `llm` registry. Each entry is keyed by `name` and
/// describes a provider plus its credentials; the models it can serve
/// live in [`Self::model_list`]. Multiple entries can target the same
/// provider (e.g. one `openai`-based "gpt-5" entry and another for
/// "gpt-4o").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmEntry {
    /// Stable identifier for this entry. Referenced by `default-llm`
    /// and by the operator CLI. Must be unique within the `llm` list.
    pub name: LlmEntryName,
    /// Provider id, e.g. `"openai"`, `"anthropic"`, `"gemini"`,
    /// `"minimax"`, `"openai-subscription"`.
    pub provider: String,
    /// Default model id, e.g. `"gpt-4o"` — the one this entry resolves to
    /// when a session pins the entry without choosing a specific model.
    /// Must name one of [`Self::models`].
    pub model: String,
    /// Every model this entry can serve, in the order the chat header's
    /// picker lists them. [`Self::models`] prepends [`Self::model`] when
    /// it isn't listed here, so listing only the *extra* models is
    /// equivalent to listing the default first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_list: Vec<LlmModelSpec>,
    /// A cheaper/faster model for lightweight auxiliary calls (the Bash
    /// risk judges, WebFetch's page summary, title generation). Must name
    /// one of [`Self::model_list`]. `None` = no lite model for this entry;
    /// resolution then falls through to `agent.model_tiers[lite]` and
    /// finally to the session's own client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lite_model: Option<String>,
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
    /// Reasoning effort for providers that expose it (currently only
    /// `openai-subscription` Codex Responses). One of `none`,
    /// `minimal`, `low`, `medium`, `high`, `xhigh`. The provider
    /// silently clamps to whatever the chosen model supports.
    /// `None` lets the provider pick a sensible default.
    ///
    /// Entry-level rather than per-model because it is a preference, not
    /// a fact about a model: a session's own thinking-level pick
    /// (`sessions.last_effort`) overrides it per request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl LlmEntry {
    /// Every model this entry serves, normalized: [`Self::model`] first
    /// when [`Self::model_list`] doesn't already contain it, then the
    /// list as written. An operator who lists the default explicitly
    /// keeps full control of the picker order.
    pub fn models(&self) -> Vec<LlmModelSpec> {
        let mut out = Vec::with_capacity(self.model_list.len() + 1);
        if !self.model_list.iter().any(|s| s.model == self.model) {
            out.push(LlmModelSpec::bare(self.model.clone()));
        }
        out.extend(self.model_list.iter().cloned());
        out
    }

    /// The spec for `model` among [`Self::models`], or `None` when this
    /// entry doesn't serve it.
    pub fn spec_for(&self, model: &str) -> Option<LlmModelSpec> {
        self.models().into_iter().find(|s| s.model == model)
    }

    /// Mutable spec for [`Self::model`], materialising a bare entry at the
    /// front of `model_list` when the default isn't listed yet. Lets an
    /// interactive editor set one per-model override without the operator
    /// hand-writing `model_list` first.
    pub fn default_spec_mut(&mut self) -> &mut LlmModelSpec {
        let idx = match self.model_list.iter().position(|s| s.model == self.model) {
            Some(i) => i,
            None => {
                self.model_list
                    .insert(0, LlmModelSpec::bare(self.model.clone()));
                0
            }
        };
        &mut self.model_list[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_from(json: &str) -> LlmEntry {
        serde_json::from_str(json).expect("entry must parse")
    }

    #[test]
    fn model_specs_carry_per_model_overrides() {
        let e = entry_from(
            r#"{"name":"a","provider":"openai","model":"gpt-5",
                "model_list":[{"model":"gpt-5-mini","context_window":128000,
                               "pricing":{"input_per_1m_tokens":1000000}},
                              {"model":"gpt-5-nano","supports_vision":false}]}"#,
        );
        assert_eq!(
            e.models()
                .iter()
                .map(|s| s.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5", "gpt-5-mini", "gpt-5-nano"]
        );
        let mini = e.spec_for("gpt-5-mini").expect("listed model");
        assert_eq!(mini.context_window, Some(128_000));
        assert!(mini.pricing.is_some());
        assert_eq!(
            e.spec_for("gpt-5-nano").and_then(|s| s.supports_vision),
            Some(false)
        );
        // The default model was never listed, so it carries no overrides.
        assert_eq!(e.spec_for("gpt-5"), Some(LlmModelSpec::bare("gpt-5")));
    }

    /// Listing only the extras is equivalent to listing the default first
    /// — that equivalence is the whole normalization rule.
    #[test]
    fn omitting_the_default_normalizes_to_listing_it_first() {
        let implicit = entry_from(
            r#"{"name":"a","provider":"openai","model":"gpt-5",
                "model_list":[{"model":"gpt-5-mini"}]}"#,
        );
        let explicit = entry_from(
            r#"{"name":"a","provider":"openai","model":"gpt-5",
                "model_list":[{"model":"gpt-5"},{"model":"gpt-5-mini"}]}"#,
        );
        assert_eq!(implicit.models(), explicit.models());
        assert_eq!(
            implicit
                .models()
                .iter()
                .map(|s| s.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5", "gpt-5-mini"]
        );
    }

    #[test]
    fn listed_default_is_not_duplicated_and_keeps_operator_order() {
        let e = entry_from(
            r#"{"name":"a","provider":"openai","model":"gpt-5",
                "model_list":[{"model":"gpt-5-mini"},{"model":"gpt-5"}]}"#,
        );
        assert_eq!(
            e.models()
                .iter()
                .map(|s| s.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5-mini", "gpt-5"],
            "an explicitly listed default keeps its position"
        );
    }

    #[test]
    fn empty_model_list_yields_just_the_default() {
        let e = entry_from(r#"{"name":"a","provider":"openai","model":"gpt-5"}"#);
        assert_eq!(e.models(), vec![LlmModelSpec::bare("gpt-5")]);
    }

    /// Every admin mutation rewrites the whole config file, so a spec with
    /// no overrides must not grow null-valued keys on the way back out.
    #[test]
    fn specs_round_trip_without_gaining_null_keys() {
        let e = entry_from(
            r#"{"name":"a","provider":"openai","model":"gpt-5",
                "model_list":[{"model":"gpt-5-mini"},
                              {"model":"gpt-5-nano","context_window":64000}]}"#,
        );
        let json = serde_json::to_value(&e).expect("serialize");
        let list = json["model_list"].as_array().expect("array");
        assert_eq!(list[0], serde_json::json!({"model": "gpt-5-mini"}));
        assert_eq!(
            list[1],
            serde_json::json!({"model": "gpt-5-nano", "context_window": 64000})
        );
    }

    /// The entry-level overrides `model_list` replaced are gone, not
    /// tombstoned: a config that still carries them parses, and the keys
    /// are ignored like any other unknown field.
    #[test]
    fn old_entry_level_override_keys_are_ignored() {
        let e = entry_from(
            r#"{"name":"a","provider":"openai","model":"gpt-5",
                "context_window":200000,"supports_vision":true,
                "pricing":{"input_per_1m_tokens":1000000}}"#,
        );
        assert_eq!(e.models(), vec![LlmModelSpec::bare("gpt-5")]);
        let json = serde_json::to_value(&e).expect("serialize");
        assert!(json.get("context_window").is_none(), "{json}");
        assert!(json.get("pricing").is_none(), "{json}");
    }
}
