//! Configuration for the pluggable web-search provider (`baybo-search`).
//!
//! Shape mirrors [`crate::memory`]: a master switch, a provider enum, and a
//! reference to a credential that lives in the vault rather than here. The
//! knobs are all operator-facing — the model chooses a query and a filter,
//! never a provider, a result count, or a locale.

use serde::{Deserialize, Serialize};

/// Ceiling on [`WebSearchConfig::max_results`]. Every shipped provider caps a
/// single page at 20 (Tavily `max_results`, Brave `count`, SearXNG's first
/// page), so the validator refuses a larger value instead of letting the
/// provider silently clamp it.
///
/// Lives here rather than in `baybo-search` because `validate()` needs it and
/// `baybo-config` is a near-leaf: it must not depend on a domain crate.
pub const MAX_RESULTS_CEILING: usize = 20;

/// Results requested per query when unset. Below every provider's page cap,
/// and small enough that one search never dominates the agent's context.
pub const DEFAULT_MAX_RESULTS: usize = 8;

/// Which backend the runtime constructs for the single `WebSearch` tool.
/// Defaults to [`WebSearchProvider::Noop`], which registers no tool at all.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchProvider {
    #[default]
    Noop,
    Tavily,
    Brave,
    Searxng,
}

impl WebSearchProvider {
    /// Secret name this provider's key is looked up under when
    /// [`WebSearchConfig::api_key_name`] is unset.
    ///
    /// Per-provider rather than one shared name: with a single
    /// `WEB_SEARCH_API_KEY`, flipping `provider` would resolve the *previous*
    /// provider's key and send it to the new endpoint, producing a 401 whose
    /// remedy ("add the secret") the operator has already done.
    pub fn default_api_key_name(&self) -> Option<&'static str> {
        match self {
            Self::Noop | Self::Searxng => None,
            Self::Tavily => Some("TAVILY_API_KEY"),
            Self::Brave => Some("BRAVE_API_KEY"),
        }
    }

    /// Whether this provider cannot run without a credential. A self-hosted
    /// SearXNG has no key at all, so boot must not treat its absence as a
    /// misconfiguration.
    pub fn requires_api_key(&self) -> bool {
        self.default_api_key_name().is_some()
    }

    /// Whether `base_url` is mandatory. SearXNG has no hosted endpoint — the
    /// operator's own instance *is* the address.
    pub fn requires_base_url(&self) -> bool {
        matches!(self, Self::Searxng)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebSearchConfig {
    /// Master switch. **Default: `false`** (opt-in, like `browser.enable` and
    /// `memory.enabled`). While false the agent is never shown a `WebSearch`
    /// verb — a tool the model can see but never execute wastes prompt tokens
    /// and invites retry loops.
    pub enabled: bool,

    /// Which backend the runtime constructs. Ignored when [`Self::enabled`]
    /// is false.
    pub provider: WebSearchProvider,

    /// Name of the user secret holding this provider's API key. Resolved at
    /// startup as vault entry `user_env.<api_key_name>` (managed via
    /// `baybo secret add <name>`), then the process env var of the same name.
    /// `None` → [`WebSearchProvider::default_api_key_name`]. The config holds
    /// a **reference**, never a key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_name: Option<String>,

    /// Override the provider's REST base URL — an enterprise gateway, or the
    /// address of a self-hosted instance. Required for
    /// [`WebSearchProvider::Searxng`]; `None` elsewhere means the provider's
    /// own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Results requested per query. The model cannot set this.
    pub max_results: usize,

    /// Region hint passed to providers that localize results — Brave defaults
    /// to `us`, which silently biases a non-US deployment with nothing in the
    /// output to reveal it. `None` leaves the provider's own default in place.
    ///
    /// **The format is provider-specific and is forwarded verbatim.** Brave
    /// wants an ISO 3166-1 alpha-2 code (`jp`); Tavily wants the full
    /// lowercase country name (`japan`) and rejects a 2-letter code with a
    /// 400. Not validated here — the vocabularies are long, provider-owned,
    /// and change without notice, so a stale allowlist in this crate would
    /// reject values that work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,

    /// Result language hint passed to providers that accept one (Brave
    /// `search_lang`, SearXNG `language`). `None` leaves the provider's
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// Hosts whose results are dropped before the model sees them, applied to
    /// every search regardless of what the model asked for. Operator policy:
    /// the model's own `blocked_domains` can narrow this, never widen it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_domains: Vec<String>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: WebSearchProvider::default(),
            api_key_name: None,
            base_url: None,
            max_results: DEFAULT_MAX_RESULTS,
            country: None,
            language: None,
            blocked_domains: Vec::new(),
        }
    }
}

impl WebSearchConfig {
    /// The secret name this deployment's key is stored under, or `None` for a
    /// keyless provider.
    pub fn resolved_api_key_name(&self) -> Option<&str> {
        match &self.api_key_name {
            Some(name) => Some(name.as_str()),
            None => self.provider.default_api_key_name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off() {
        let c = WebSearchConfig::default();
        assert!(!c.enabled, "web search is opt-in like browser and memory");
        assert_eq!(c.provider, WebSearchProvider::Noop);
        assert_eq!(c.max_results, DEFAULT_MAX_RESULTS);
        assert!(c.api_key_name.is_none());
        assert!(c.blocked_domains.is_empty());
    }

    #[test]
    fn empty_object_yields_defaults() {
        let c: WebSearchConfig = serde_json::from_str("{}").expect("parse");
        assert_eq!(c, WebSearchConfig::default());
    }

    #[test]
    fn partial_section_keeps_sibling_defaults() {
        let c: WebSearchConfig =
            serde_json::from_str(r#"{"enabled":true,"provider":"tavily"}"#).expect("parse");
        assert!(c.enabled);
        assert_eq!(c.max_results, DEFAULT_MAX_RESULTS);
    }

    #[test]
    fn default_omits_optional_fields_when_serialized() {
        let json = serde_json::to_string(&WebSearchConfig::default()).expect("serialize");
        assert!(!json.contains("api_key_name"), "None elided: {json}");
        assert!(!json.contains("base_url"), "None elided: {json}");
        assert!(!json.contains("blocked_domains"), "empty elided: {json}");
        assert!(json.contains("enabled"));
    }

    #[test]
    fn provider_round_trips_lowercase() {
        for (text, provider) in [
            ("tavily", WebSearchProvider::Tavily),
            ("brave", WebSearchProvider::Brave),
            ("searxng", WebSearchProvider::Searxng),
            ("noop", WebSearchProvider::Noop),
        ] {
            let c: WebSearchConfig =
                serde_json::from_str(&format!(r#"{{"provider":"{text}"}}"#)).expect("parse");
            assert_eq!(c.provider, provider);
            let json = serde_json::to_string(&c).expect("serialize");
            assert!(
                json.contains(&format!(r#""provider":"{text}""#)),
                "got {json}"
            );
        }
    }

    /// The whole point of a per-provider default: two providers must never
    /// resolve the same secret.
    #[test]
    fn each_keyed_provider_has_its_own_default_secret_name() {
        assert_eq!(
            WebSearchProvider::Tavily.default_api_key_name(),
            Some("TAVILY_API_KEY")
        );
        assert_eq!(
            WebSearchProvider::Brave.default_api_key_name(),
            Some("BRAVE_API_KEY")
        );
        assert_eq!(WebSearchProvider::Searxng.default_api_key_name(), None);
        assert!(!WebSearchProvider::Searxng.requires_api_key());
        assert!(WebSearchProvider::Searxng.requires_base_url());
        assert!(!WebSearchProvider::Tavily.requires_base_url());
    }

    #[test]
    fn an_explicit_key_name_overrides_the_provider_default() {
        let c = WebSearchConfig {
            provider: WebSearchProvider::Tavily,
            api_key_name: Some("MY_KEY".into()),
            ..Default::default()
        };
        assert_eq!(c.resolved_api_key_name(), Some("MY_KEY"));

        let c = WebSearchConfig {
            provider: WebSearchProvider::Tavily,
            ..Default::default()
        };
        assert_eq!(c.resolved_api_key_name(), Some("TAVILY_API_KEY"));
    }
}
