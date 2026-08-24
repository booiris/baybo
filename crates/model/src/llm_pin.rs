//! What an agent runs on: the `baybo.json` LLM entry, the model within it,
//! and how hard that model should think.
//!
//! The three travel as one value because two of them mean nothing alone. A
//! model id is a model *of some entry* — `o3` names nothing until you know
//! which entry serves it — and a rung is translated into the provider's own
//! effort dialect, which is a property of the entry too. A writer that set
//! two of the three would leave a pin whose model belongs to an entry the pin
//! no longer names, and the run would silently fall back to the entry default
//! while the board still displayed the model nobody is running.
//!
//! `None` is "inherit" at every level, and each level inherits from something
//! different: no entry follows `default-llm`, no model follows the entry's
//! own `model`, no effort follows the entry's own `reasoning_effort`.
//!
//! The invariant `model.is_some() => entry.is_some()` is not enforced by this
//! type: the fields stay public so the sqlite row maps onto them one column
//! each, exactly as `SessionState`'s `last_llm` / `last_model` / `last_effort`
//! do. Its one home is the gateway's `validate_llm_pin`, which every write
//! surface goes through.

use crate::LlmEntryName;

/// An LLM pin: entry, the model within it, and the thinking rung.
///
/// [`Default`] is the fully-unpinned pin — follow the deployment's defaults
/// at every level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmPin {
    /// `baybo.json` entry name, or `None` to follow `default-llm`.
    pub entry: Option<LlmEntryName>,
    /// A model id from `entry`'s `[model] + model_list`, or `None` for that
    /// entry's own default model. Only meaningful alongside `entry`.
    pub model: Option<String>,
    /// A rung of baybo's thinking ladder (`baybo_llm::effort::ReasoningEffort`,
    /// canonically spelled), or `None` for the entry's configured default.
    /// Held as a string here for the same reason `SessionState::last_effort`
    /// is: `baybo-model` sits below `baybo-llm`, and a rung is stored, not
    /// interpreted, until it reaches the provider.
    pub effort: Option<String>,
}

impl LlmPin {
    /// Follow the deployment's defaults at every level — what the built-in
    /// agent is held at, and what clearing a pin writes.
    pub fn unpinned() -> Self {
        Self::default()
    }

    /// Whether this pin chooses nothing at all.
    pub fn is_unpinned(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_pin_chooses_nothing() {
        let pin = LlmPin::unpinned();
        assert!(pin.is_unpinned());
        assert_eq!(pin, LlmPin::default());
    }

    /// Any one level set makes the pin a pin — the effort rung included,
    /// which is the level a caller is most likely to forget carrying.
    #[test]
    fn any_level_set_is_a_pin() {
        for pin in [
            LlmPin {
                entry: Some(LlmEntryName::from("fast")),
                ..Default::default()
            },
            LlmPin {
                model: Some("o3".into()),
                ..Default::default()
            },
            LlmPin {
                effort: Some("high".into()),
                ..Default::default()
            },
        ] {
            assert!(!pin.is_unpinned(), "{pin:?} chooses something");
        }
    }
}
