//! Cross-crate model-tier enum used by:
//!  - `spawn_subagent` (parent LLM picks a tier per spawn)
//!  - `SubagentProfile.default_tier` (per-type default)
//!  - `LlmClientPool` (resolves tier → entry name via `baybo.json`)
//!
//! The tier abstraction lets the parent LLM express intent ("this is a
//! cheap exploration", "this is a deep reasoning task") without knowing
//! which specific `baybo.json` entry name happens to be bound to that
//! tier in the current deployment. Mapping lives in config.
//!
//! [`ModelTier::Lite`] does double duty: it is also the process-wide
//! fallback for the agent's auxiliary LLM calls when the resolved entry
//! declares no `lite_model` of its own. Re-pointing it therefore moves
//! both the cheap subagent tier and the risk judges / page summariser /
//! title generator. An operator who wants them apart sets a per-entry
//! `lite_model`, which outranks the tier.

use serde::{Deserialize, Serialize};

/// Three coarse model-cost tiers. Operators map each tier to a concrete
/// `llm[*].name` in `baybo.json`; if a tier is unmapped the resolver
/// falls back to the pool default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Cheapest tier — fast iteration, exploration, low-stakes work,
    /// and the fallback for auxiliary LLM calls.
    ///
    /// The `fast` alias is what this tier used to be called; a
    /// `model_tiers` map deserializes its **keys** through this enum, so
    /// dropping the alias would turn every config written before the
    /// rename into a hard load failure.
    #[serde(alias = "fast")]
    Lite,
    /// Mid tier — general-purpose default.
    Balanced,
    /// Most capable tier — deep reasoning, code review, planning.
    Deep,
}

impl ModelTier {
    /// Stable lower-case label used in JSON / on-disk frontmatter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Balanced => "balanced",
            Self::Deep => "deep",
        }
    }

    /// Parse from a user-facing label. Case-insensitive.
    pub fn parse(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "lite" | "fast" => Some(Self::Lite),
            "balanced" | "default" | "mid" => Some(Self::Balanced),
            "deep" | "opus" | "max" => Some(Self::Deep),
            _ => None,
        }
    }

    /// All three tiers in lite→deep order. Useful for config printers
    /// and the `spawn_subagent` description renderer.
    pub fn all() -> [Self; 3] {
        [Self::Lite, Self::Balanced, Self::Deep]
    }
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_serde() {
        for t in ModelTier::all() {
            let json = serde_json::to_string(&t).unwrap();
            let back: ModelTier = serde_json::from_str(&json).unwrap();
            assert_eq!(t, back);
        }
    }

    #[test]
    fn parse_accepts_aliases_and_is_case_insensitive() {
        assert_eq!(ModelTier::parse("Lite"), Some(ModelTier::Lite));
        assert_eq!(ModelTier::parse(" BALANCED "), Some(ModelTier::Balanced));
        assert_eq!(ModelTier::parse("default"), Some(ModelTier::Balanced));
        assert_eq!(ModelTier::parse("opus"), Some(ModelTier::Deep));
        assert_eq!(ModelTier::parse("deep"), Some(ModelTier::Deep));
        assert!(ModelTier::parse("ultradeep").is_none());
    }

    #[test]
    fn lowercase_labels_match_as_str() {
        assert_eq!(ModelTier::Lite.as_str(), "lite");
        assert_eq!(ModelTier::Balanced.as_str(), "balanced");
        assert_eq!(ModelTier::Deep.as_str(), "deep");
    }

    /// `fast` is the pre-rename spelling. It has to keep working in both
    /// directions that read a label: on-disk subagent frontmatter goes
    /// through `parse`, and `agent.model_tiers` deserializes its map
    /// **keys** through serde — an unaliased rename would turn an
    /// existing `baybo.json` into a config-load failure, not a warning.
    #[test]
    fn the_pre_rename_fast_spelling_still_resolves() {
        assert_eq!(ModelTier::parse("fast"), Some(ModelTier::Lite));
        assert_eq!(ModelTier::parse("FAST"), Some(ModelTier::Lite));
        let from_json: ModelTier = serde_json::from_str(r#""fast""#).expect("alias deserializes");
        assert_eq!(from_json, ModelTier::Lite);
        let as_key: std::collections::HashMap<ModelTier, String> =
            serde_json::from_str(r#"{"fast":"cheap-entry"}"#).expect("alias works as a map key");
        assert_eq!(
            as_key.get(&ModelTier::Lite).map(String::as_str),
            Some("cheap-entry")
        );
    }

    /// …but it is not what we write back out.
    #[test]
    fn lite_is_the_canonical_serialization() {
        assert_eq!(
            serde_json::to_string(&ModelTier::Lite).expect("serialize"),
            r#""lite""#
        );
    }
}
