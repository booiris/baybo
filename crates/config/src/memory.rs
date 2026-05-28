//! Configuration for the pluggable memory subsystem (`aura-memory`).
//!
//! Core-wiring knobs are typed; per-plugin settings ride in an opaque
//! [`extra`](MemoryConfig::extra) bag — a deliberate, documented exception to
//! the "typed over `Value`" rule, because the registered memory implementation
//! is a plug-in whose own configuration the core cannot know. The runtime
//! unpacks the typed fields and forwards `extra` verbatim to the implementation.

use serde::{Deserialize, Serialize};

use aura_model::LlmEntryName;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct MemoryConfig {
    /// Master switch. **Default: `false`** (off — like `browser.enable`). When
    /// false the runtime registers the no-op memory: no recall, no write, no
    /// memory tools. A real backend opts in by flipping this to `true`. Until
    /// one ships, the runtime is no-op regardless of this flag.
    pub enabled: bool,

    /// Name of the `llm` entry the memory implementation uses for its
    /// salience / extraction calls. `None` → fall back to `default-llm`.
    /// Mirrors how the agent loop resolves its model, so memory work can target
    /// a cheaper model than the chat path.
    #[serde(rename = "llm", skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmEntryName>,

    /// Opaque, implementation-defined settings forwarded verbatim to the
    /// registered memory plug-in. A deliberate, documented exception to the
    /// "typed over `Value`" rule: the plug-in's own configuration is genuinely
    /// opaque to the core, which only passes it through. **Default: `null`.**
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let c = MemoryConfig::default();
        assert!(
            !c.enabled,
            "memory defaults off (opt-in like browser.enable)"
        );
        assert!(c.llm.is_none());
        assert!(c.extra.is_null());
    }

    #[test]
    fn empty_object_yields_defaults() {
        let c: MemoryConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c, MemoryConfig::default());
    }

    #[test]
    fn default_omits_optional_fields_when_serialized() {
        let json = serde_json::to_string(&MemoryConfig::default()).unwrap();
        assert!(!json.contains("llm"), "None llm elided");
        assert!(!json.contains("extra"), "null extra elided");
        assert!(json.contains("enabled"));
    }

    #[test]
    fn round_trip_with_extra_passthrough() {
        let c = MemoryConfig {
            enabled: true,
            llm: Some(LlmEntryName::from("cheap-model")),
            extra: serde_json::json!({ "max_entries": 5000, "namespace": "team-a" }),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: MemoryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.extra["max_entries"], 5000);
    }
}
