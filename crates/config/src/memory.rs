//! Configuration for the pluggable memory subsystem (`baybo-memory`).
//!
//! Core-wiring knobs are typed; per-plugin settings ride in an opaque
//! [`extra`](MemoryConfig::extra) bag — a deliberate, documented exception to
//! the "typed over `Value`" rule, because the registered memory implementation
//! is a plug-in whose own configuration the core cannot know. The runtime
//! unpacks the typed fields and forwards `extra` verbatim to the implementation.

use serde::{Deserialize, Serialize};

use baybo_model::LlmEntryName;

/// Which backend the runtime should construct for the single `Arc<dyn Memory>`
/// slot. The trait is single-pluggable; the runtime dispatches on this enum.
/// Defaults to [`MemoryProvider::Noop`], which registers `NoopMemory` and
/// leaves every memory hook inert (no recall, no write, no billing).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryProvider {
    #[default]
    Noop,
    Mem0,
    OpenViking,
}

/// Default dream schedule: twice a week, 04:00 in the job's own timezone.
///
/// Named days, not numbers: the pinned `cron` crate rejects a day-of-week
/// `0` and counts Sunday as `1`, so every numeric form of "Sunday and
/// Wednesday" is either a parse error or the wrong pair of days.
pub const DEFAULT_DREAM_SCHEDULE: &str = "0 4 * * SUN,WED";

/// The memory that ships with the assistant: a tree per agent at
/// `<workspace>/personas/<id>/memory/`, and the scheduled dream pass that
/// tends it.
///
/// Independent of the pluggable [`MemoryProvider`] above, and named for the
/// real distinction between them — both are stores of remembered things;
/// only this one is built in rather than an integration to configure. A
/// deployment can run both, either, or neither. **On by default.**
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BuiltinMemoryConfig {
    /// Whether the memory index is injected into the system prompt and the
    /// dream job runs. **Default: `true`.** Turning it off leaves every
    /// memory file on disk untouched — it only stops reading and writing
    /// them.
    pub enabled: bool,

    /// The scheduled consolidation pass.
    pub dream: DreamConfig,
}

impl Default for BuiltinMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dream: DreamConfig::default(),
        }
    }
}

/// Scheduling of the dream pass — the recurring job that consolidates
/// memory and rebalances it against the profile identity files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DreamConfig {
    /// Cron expression for the built-in dream job, in the job's timezone.
    /// Applied to the seeded job at boot; an operator who reschedules the
    /// job from the UI is not overridden (see the cron store's builtin
    /// seed).
    pub schedule: String,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            schedule: DEFAULT_DREAM_SCHEDULE.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct MemoryConfig {
    /// Master switch. **Default: `false`** (off — like `browser.enable`). When
    /// false the runtime registers the no-op memory regardless of [`provider`]:
    /// no recall, no write, no memory tools. A real backend opts in by flipping
    /// this to `true` and selecting a [`MemoryProvider`].
    ///
    /// [`provider`]: Self::provider
    pub enabled: bool,

    /// Which backend the runtime constructs. Ignored when [`enabled`] is false.
    /// Defaults to [`MemoryProvider::Noop`].
    ///
    /// [`enabled`]: Self::enabled
    pub provider: MemoryProvider,

    /// Name of the `llm` entry the memory implementation uses for its
    /// salience / extraction calls. `None` → fall back to `default-llm`.
    /// Mirrors how the agent loop resolves its model, so memory work can target
    /// a cheaper model than the chat path. The bundled `mem0` and `openviking`
    /// backends do their extraction server-side and ignore this field.
    #[serde(rename = "llm", skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmEntryName>,

    /// Opaque, implementation-defined settings forwarded verbatim to the
    /// registered memory plug-in. A deliberate, documented exception to the
    /// "typed over `Value`" rule: the plug-in's own configuration is genuinely
    /// opaque to the core, which only passes it through. **Default: `null`.**
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,

    /// The memory that ships with the assistant, plus its dream pass.
    /// Orthogonal to every field above: [`enabled`] and [`provider`] govern
    /// the pluggable backend only.
    ///
    /// [`enabled`]: Self::enabled
    /// [`provider`]: Self::provider
    pub builtin: BuiltinMemoryConfig,
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
        assert_eq!(c.provider, MemoryProvider::Noop);
        assert!(c.llm.is_none());
        assert!(c.extra.is_null());
    }

    #[test]
    fn builtin_memory_defaults_on() {
        let c = MemoryConfig::default();
        assert!(
            c.builtin.enabled,
            "the built-in memory tree is part of the assistant, not an opt-in integration"
        );
        assert_eq!(c.builtin.dream.schedule, DEFAULT_DREAM_SCHEDULE);
    }

    #[test]
    fn builtin_memory_survives_an_absent_section() {
        // A config written before this feature existed must not silently
        // switch it off.
        let c: MemoryConfig = serde_json::from_str(r#"{"enabled":true,"provider":"mem0"}"#)
            .expect("legacy config parses");
        assert!(c.builtin.enabled);
        assert_eq!(c.builtin.dream.schedule, DEFAULT_DREAM_SCHEDULE);
    }

    #[test]
    fn builtin_memory_can_be_switched_off_without_touching_the_backend() {
        let c: MemoryConfig =
            serde_json::from_str(r#"{"builtin":{"enabled":false}}"#).expect("parse");
        assert!(!c.builtin.enabled);
        // Partial sections keep their sibling defaults.
        assert_eq!(c.builtin.dream.schedule, DEFAULT_DREAM_SCHEDULE);
        // …and the pluggable backend is unaffected.
        assert_eq!(c.provider, MemoryProvider::Noop);
    }

    #[test]
    fn dream_schedule_is_overridable() {
        let c: MemoryConfig =
            serde_json::from_str(r#"{"builtin":{"dream":{"schedule":"0 5 * * MON"}}}"#)
                .expect("parse");
        assert_eq!(c.builtin.dream.schedule, "0 5 * * MON");
        assert!(
            c.builtin.enabled,
            "an override must not disable the feature"
        );
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
            provider: MemoryProvider::Mem0,
            llm: Some(LlmEntryName::from("cheap-model")),
            extra: serde_json::json!({ "max_entries": 5000, "namespace": "team-a" }),
            builtin: BuiltinMemoryConfig::default(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: MemoryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.extra["max_entries"], 5000);
    }

    #[test]
    fn provider_deserializes_lowercase() {
        let cfg: MemoryConfig =
            serde_json::from_str(r#"{"enabled":true,"provider":"openviking"}"#).unwrap();
        assert_eq!(cfg.provider, MemoryProvider::OpenViking);

        let cfg: MemoryConfig =
            serde_json::from_str(r#"{"enabled":true,"provider":"mem0"}"#).unwrap();
        assert_eq!(cfg.provider, MemoryProvider::Mem0);

        let cfg: MemoryConfig =
            serde_json::from_str(r#"{"enabled":true,"provider":"noop"}"#).unwrap();
        assert_eq!(cfg.provider, MemoryProvider::Noop);
    }

    #[test]
    fn provider_serializes_lowercase() {
        let c = MemoryConfig {
            enabled: true,
            provider: MemoryProvider::OpenViking,
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""provider":"openviking""#), "got: {json}");
    }
}
