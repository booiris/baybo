//! Baybo's thinking ladder, and how each provider spells its rungs.
//!
//! Every vendor invented its own effort vocabulary — different names,
//! different nesting, different sets. Operators pick from one ladder here and
//! it is translated per provider on the way out.
//!
//! Translation is not clamping. Clamping rewrites a value that was already in
//! the provider's vocabulary, which silently bills a level nobody asked for.
//! Translation carries a rung from *our* vocabulary into *theirs*, and a rung
//! a dialect cannot express is an error the operator sees — never a quiet
//! round to the nearest neighbour.
//!
//! Whether a specific model accepts an expressible rung stays the vendor's
//! call: we send it and let their API answer. Several already normalise on
//! their side (DeepSeek folds `low`/`medium` into `high`), and second-guessing
//! that is how effort levels start lying again.

use std::fmt;

use serde_json::{Value, json};

/// Top-level field name used by the OpenAI Chat Completions dialect and every
/// provider that speaks it.
const OPENAI_FIELD: &str = "reasoning_effort";

/// Codex Responses' spelling of [`ReasoningEffort::Off`]. Its body builder
/// turns this into an omitted `reasoning` field.
const CODEX_OFF: &str = "none";

/// The rung a call the session never asked for runs at: a compaction
/// summary, a progress observation. Their output is short and mechanical
/// and their input is a transcript the session already paid to think
/// about, so inheriting the session's rung buys nothing and is billed at
/// the session's rate.
pub const OUT_OF_BAND_EFFORT: ReasoningEffort = ReasoningEffort::Low;

/// How hard the model should think, on baybo's own ladder.
///
/// The union of what the wired providers offer, ordered cheapest first, so
/// pickers and comparisons can rely on the declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReasoningEffort {
    /// Reasoning off entirely.
    Off,
    /// The shallowest reasoning a provider offers as a distinct level —
    /// below `Low`, not the same thing as off.
    Minimal,
    Low,
    Medium,
    High,
    /// Between `High` and `Max`. Both Codex and Anthropic recommend it for
    /// coding and agentic work, which is why the ladder carries it rather
    /// than folding it into `High`.
    XHigh,
    Max,
}

impl ReasoningEffort {
    /// Every rung, cheapest first. Drives operator-facing pickers.
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// Baybo's name for this rung — the string that appears in config, in a
    /// session's pin, and on a cost row.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parse an operator-typed level, case-insensitively. `None` when the
    /// string is not a rung — the caller decides whether that is an error or
    /// a value to forward untouched.
    ///
    /// `none` is accepted as a synonym for `off`: it is what Codex calls the
    /// same thing, and it is what operators who configured this before the
    /// ladder existed already have on disk.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("none") {
            return Some(Self::Off);
        }
        Self::ALL
            .into_iter()
            .find(|l| s.eq_ignore_ascii_case(l.as_str()))
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the operator actually configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EffortPick {
    /// A rung on the ladder — translated per provider.
    Canonical(ReasoningEffort),
    /// Not a rung. Forwarded to the provider verbatim so a level baybo hasn't
    /// learned yet is still reachable without waiting on a release. Nothing
    /// validates it; the provider is the one that answers.
    Raw(String),
}

impl EffortPick {
    pub(crate) fn parse(s: &str) -> Self {
        match ReasoningEffort::parse(s) {
            Some(level) => Self::Canonical(level),
            None => Self::Raw(s.trim().to_string()),
        }
    }

    /// The name this pick carries on a cost row: the canonical rung when
    /// there is one, so spend stays comparable across providers that spell
    /// the same depth differently.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Canonical(level) => level.as_str(),
            Self::Raw(raw) => raw,
        }
    }
}

/// A rung this provider's dialect has no way to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsupportedLevel {
    pub(crate) level: ReasoningEffort,
    pub(crate) dialect: &'static str,
    /// The rungs this dialect can express, for the error message.
    pub(crate) supported: &'static [ReasoningEffort],
}

impl fmt::Display for UnsupportedLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.supported.iter().map(|l| l.as_str()).collect();
        write!(
            f,
            "reasoning effort `{}` has no equivalent on {} — this provider understands: {}",
            self.level,
            self.dialect,
            names.join(", ")
        )
    }
}

use ReasoningEffort as R;

/// Rungs each dialect can express, and the name it uses. Deliberately a
/// table: adding a provider means writing its row down, not inferring one.
const OPENAI_LEVELS: &[ReasoningEffort] = &[R::Low, R::Medium, R::High, R::XHigh, R::Max];
const ANTHROPIC_LEVELS: &[ReasoningEffort] = &[R::Low, R::Medium, R::High, R::XHigh, R::Max];
const GEMINI_LEVELS: &[ReasoningEffort] = &[R::Minimal, R::Low, R::Medium, R::High];
/// Codex Responses spans the whole ladder across its model families, but
/// which rungs a *given* model takes is narrower and moves — `gpt-5.6-sol`
/// answers `level 'max' not supported, valid levels: low, medium, high,
/// xhigh`, while other 5.6 models take `max`. That per-model truth is the
/// API's to state, and it says so in a plain error; encoding a snapshot of it
/// here would just be a table that silently goes stale.
const CODEX_LEVELS: &[ReasoningEffort] = &[
    R::Off,
    R::Minimal,
    R::Low,
    R::Medium,
    R::High,
    R::XHigh,
    R::Max,
];

/// How a provider expects the effort level, if it takes one at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortWire {
    /// Top-level `reasoning_effort` — OpenAI Chat Completions and the many
    /// providers that mirror its request shape.
    OpenAiCompatible,
    /// `output_config.effort` — Anthropic Messages API. Note this is a
    /// sibling of `thinking`, not a field inside it.
    Anthropic,
    /// `generationConfig.thinkingConfig.thinkingLevel` — Gemini. rig treats
    /// `generationConfig` as the base config and layers the request's own
    /// temperature / max-output-tokens on top, so nesting here is safe.
    Gemini,
    /// The provider builds its own request body and consumes the level
    /// directly, so nothing rides along in `additional_params`.
    ProviderNative,
    /// No effort control is wired for this provider. Not a claim that the
    /// vendor lacks one — most now have some form of it — only that baybo
    /// does not yet send it, so the operator's setting is inert here.
    Unwired,
}

impl EffortWire {
    fn dialect(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "the OpenAI `reasoning_effort` dialect",
            Self::Anthropic => "Anthropic `output_config.effort`",
            Self::Gemini => "Gemini `thinkingConfig.thinkingLevel`",
            Self::ProviderNative => "Codex Responses `reasoning.effort`",
            Self::Unwired => "this provider",
        }
    }

    /// The rungs this dialect can express. Drives the operator-facing
    /// pickers, so a provider is never offered a level it cannot say.
    pub fn levels(self) -> &'static [ReasoningEffort] {
        match self {
            Self::OpenAiCompatible => OPENAI_LEVELS,
            Self::Anthropic => ANTHROPIC_LEVELS,
            Self::Gemini => GEMINI_LEVELS,
            Self::ProviderNative => CODEX_LEVELS,
            Self::Unwired => &[],
        }
    }

    /// This dialect's name for `level`.
    ///
    /// Only Codex can say "off", and it spells it `none`; the others disable
    /// thinking through a separate mechanism baybo does not drive, so `off`
    /// is reported as inexpressible rather than approximated by their lowest
    /// level. Every other rung shares baybo's own spelling, which is why the
    /// rest reads as a pass-through guarded by a membership test.
    pub(crate) fn level_name(
        self,
        level: ReasoningEffort,
    ) -> Result<&'static str, UnsupportedLevel> {
        if !self.levels().contains(&level) {
            return Err(UnsupportedLevel {
                level,
                dialect: self.dialect(),
                supported: self.levels(),
            });
        }
        Ok(match level {
            ReasoningEffort::Off => CODEX_OFF,
            other => other.as_str(),
        })
    }

    /// The `additional_params` fragment carrying `level`, or `None` when this
    /// provider takes its effort by another route (or none at all).
    pub(crate) fn params(self, level: &str) -> Option<Value> {
        match self {
            Self::OpenAiCompatible => Some(json!({ OPENAI_FIELD: level })),
            Self::Anthropic => Some(json!({ "output_config": { "effort": level } })),
            Self::Gemini => Some(json!({
                "generationConfig": { "thinkingConfig": { "thinkingLevel": level } }
            })),
            Self::ProviderNative | Self::Unwired => None,
        }
    }

    /// Whether an effort level reaches this provider at all. Drives what the
    /// cost ledger records: a level that is never sent must not be billed as
    /// though it were.
    pub(crate) fn carries_effort(self) -> bool {
        !matches!(self, Self::Unwired)
    }

    /// Translate a pick into the string this provider should receive.
    /// `Ok(None)` means nothing is sent, which today only happens for a
    /// provider baybo has no wiring for.
    pub(crate) fn wire_level(&self, pick: &EffortPick) -> Result<Option<String>, UnsupportedLevel> {
        if !self.carries_effort() {
            return Ok(None);
        }
        match pick {
            EffortPick::Canonical(level) => Ok(Some(self.level_name(*level)?.to_string())),
            EffortPick::Raw(raw) => Ok(Some(raw.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_is_case_insensitive_and_accepts_the_codex_spelling_of_off() {
        assert_eq!(ReasoningEffort::parse("XHigh"), Some(R::XHigh));
        assert_eq!(ReasoningEffort::parse("  max "), Some(R::Max));
        assert_eq!(ReasoningEffort::parse("none"), Some(R::Off));
        assert_eq!(ReasoningEffort::parse("off"), Some(R::Off));
        assert_eq!(ReasoningEffort::parse("ultraplus"), None);
    }

    /// A level baybo hasn't learned still reaches the provider — the ladder
    /// must not become the bottleneck on a vendor shipping a new rung.
    #[test]
    fn an_unknown_level_is_forwarded_verbatim() {
        let pick = EffortPick::parse("ultraplus");
        assert_eq!(pick, EffortPick::Raw("ultraplus".into()));
        assert_eq!(
            EffortWire::OpenAiCompatible.wire_level(&pick).unwrap(),
            Some("ultraplus".to_string())
        );
        assert_eq!(pick.label(), "ultraplus");
    }

    /// Cost rows carry the canonical rung, so spend stays comparable across
    /// providers that spell the same depth differently.
    #[test]
    fn a_canonical_pick_is_labelled_by_its_rung() {
        assert_eq!(EffortPick::parse("NONE").label(), "off");
    }

    #[test]
    fn each_dialect_nests_the_level_where_its_vendor_expects_it() {
        assert_eq!(
            EffortWire::OpenAiCompatible.params("high"),
            Some(json!({"reasoning_effort": "high"}))
        );
        assert_eq!(
            EffortWire::Anthropic.params("xhigh"),
            Some(json!({"output_config": {"effort": "xhigh"}}))
        );
        assert_eq!(
            EffortWire::Gemini.params("low"),
            Some(json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "low"}}}))
        );
    }

    #[test]
    fn routes_that_do_not_ride_additional_params_emit_nothing() {
        assert_eq!(EffortWire::ProviderNative.params("high"), None);
        assert_eq!(EffortWire::Unwired.params("high"), None);
    }

    /// `ProviderNative` still delivers the level — just through the
    /// provider's own body builder — so it must not read as "not sent".
    #[test]
    fn only_unwired_counts_as_not_sent() {
        assert!(EffortWire::ProviderNative.carries_effort());
        assert!(EffortWire::OpenAiCompatible.carries_effort());
        assert!(!EffortWire::Unwired.carries_effort());
    }

    /// The rungs that genuinely have no equivalent must refuse rather than
    /// round to a neighbour — rounding is how `low` silently became the most
    /// expensive tier before.
    #[test]
    fn an_inexpressible_rung_is_refused_not_rounded() {
        for (wire, level) in [
            (EffortWire::OpenAiCompatible, R::Off),
            (EffortWire::OpenAiCompatible, R::Minimal),
            (EffortWire::Anthropic, R::Off),
            (EffortWire::Gemini, R::XHigh),
            (EffortWire::Gemini, R::Max),
        ] {
            let err = wire
                .level_name(level)
                .expect_err("this rung has no equivalent in that dialect");
            assert_eq!(err.level, level);
            assert!(
                err.to_string().contains(level.as_str()),
                "the error has to name the rung the operator picked: {err}"
            );
        }
    }

    /// Whether a *particular* Codex model takes `max` is the API's call — it
    /// refuses with a plain message naming the levels it does take. Refusing
    /// here on a stale table would be the clamp, wearing a different hat.
    #[test]
    fn codex_can_express_every_rung_and_leaves_per_model_limits_to_the_api() {
        for level in ReasoningEffort::ALL {
            assert!(
                EffortWire::ProviderNative.level_name(level).is_ok(),
                "{level} must reach Codex for its API to judge"
            );
        }
    }

    /// Codex is the one dialect that can say "off", and it says `none` —
    /// its body builder is what turns that into an omitted field.
    #[test]
    fn off_travels_as_codex_spells_it() {
        assert_eq!(EffortWire::ProviderNative.level_name(R::Off), Ok("none"));
        assert_eq!(
            EffortWire::ProviderNative
                .wire_level(&EffortPick::Canonical(R::Off))
                .unwrap(),
            Some("none".to_string())
        );
    }

    /// Unwired providers swallow any pick without complaint — an operator
    /// setting effort on one is inert, not an error.
    #[test]
    fn an_unwired_provider_accepts_any_pick_and_sends_nothing() {
        assert_eq!(
            EffortWire::Unwired
                .wire_level(&EffortPick::Canonical(R::Max))
                .unwrap(),
            None
        );
    }
}
