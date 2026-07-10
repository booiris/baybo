use serde::{Deserialize, Serialize};

/// Per-agent reasoning-effort request for providers that support it.
/// The profile-level vocabulary; providers clamp to what the concrete
/// model accepts. `"none"` is deliberately absent — disabling reasoning
/// stays an LLM-entry configuration concern, not a persona one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

impl ReasoningEffort {
    pub const ALL: &'static [ReasoningEffort] = &[
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|e| e.as_str().eq_ignore_ascii_case(s.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_round_trips_all_variants() {
        for e in ReasoningEffort::ALL {
            assert_eq!(ReasoningEffort::parse(e.as_str()), Some(*e));
            let json = serde_json::to_string(e).unwrap();
            assert_eq!(json, format!("\"{}\"", e.as_str()));
            let back: ReasoningEffort = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *e);
        }
        assert_eq!(
            ReasoningEffort::parse("XHigh"),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(ReasoningEffort::parse("none"), None);
        assert_eq!(ReasoningEffort::parse(""), None);
        assert_eq!(ReasoningEffort::ALL.len(), 5);
    }
}
