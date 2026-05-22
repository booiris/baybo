//! Cross-crate model-tier enum used by:
//!  - `spawn_subagent` (parent LLM picks a tier per spawn)
//!  - `SubagentProfile.default_tier` (per-type default)
//!  - `LlmClientPool` (resolves tier → entry name via `aura.json`)
//!
//! The tier abstraction lets the parent LLM express intent ("this is a
//! cheap exploration", "this is a deep reasoning task") without knowing
//! which specific `aura.json` entry name happens to be bound to that
//! tier in the current deployment. Mapping lives in config.

use serde::{Deserialize, Serialize};

/// Three coarse model-cost tiers. Operators map each tier to a concrete
/// `llm[*].name` in `aura.json`; if a tier is unmapped the resolver
/// falls back to the pool default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Cheapest tier — fast iteration, exploration, low-stakes work.
    Fast,
    /// Mid tier — general-purpose default.
    Balanced,
    /// Most capable tier — deep reasoning, code review, planning.
    Deep,
}

impl ModelTier {
    /// Stable lower-case label used in JSON / on-disk frontmatter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Deep => "deep",
        }
    }

    /// Parse from a user-facing label. Case-insensitive.
    pub fn parse(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "fast" => Some(Self::Fast),
            "balanced" | "default" | "mid" => Some(Self::Balanced),
            "deep" | "opus" | "max" => Some(Self::Deep),
            _ => None,
        }
    }

    /// All three tiers in fast→deep order. Useful for config printers
    /// and the `spawn_subagent` description renderer.
    pub fn all() -> [Self; 3] {
        [Self::Fast, Self::Balanced, Self::Deep]
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
        assert_eq!(ModelTier::parse("Fast"), Some(ModelTier::Fast));
        assert_eq!(ModelTier::parse(" BALANCED "), Some(ModelTier::Balanced));
        assert_eq!(ModelTier::parse("default"), Some(ModelTier::Balanced));
        assert_eq!(ModelTier::parse("opus"), Some(ModelTier::Deep));
        assert_eq!(ModelTier::parse("deep"), Some(ModelTier::Deep));
        assert!(ModelTier::parse("ultradeep").is_none());
    }

    #[test]
    fn lowercase_labels_match_as_str() {
        assert_eq!(ModelTier::Fast.as_str(), "fast");
        assert_eq!(ModelTier::Balanced.as_str(), "balanced");
        assert_eq!(ModelTier::Deep.as_str(), "deep");
    }
}
