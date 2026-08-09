//! Observation injected when a turn has been churning one file instead of
//! advancing it.
//!
//! It is an observation, not a stop: the runtime knows the *fact* (this file
//! is back where it was) but not whether the churn is a mistake — a temporary
//! flag flipped on and off again looks identical. So it states the fact,
//! demands the model account for it, and leaves the decision there. The
//! failure this addresses was never bad reasoning; it was a model reasoning
//! correctly from a fact nobody gave it.

/// Substituted with the absolute path of the file the verdict is about.
const PATH_PLACEHOLDER: &str = "{{path}}";
/// Substituted with the count that justifies the verdict.
const COUNT_PLACEHOLDER: &str = "{{count}}";

const STATE_REVISITED: &str = r#"<system-reminder>
You have changed {{path}} {{count}} times in this turn, and the file has just returned to a state it already had earlier in this same turn. Those changes cancelled out: there is no net progress to show for them.

Before touching this file again, do one of these:
- State what NEW information you have that makes the next attempt different from the one you already undid. If you cannot name it, the next attempt will fail the same way.
- Re-check the assumption that started this. A tool error is evidence about one invocation, not proof about the world — re-read the exact error text and check it against how you invoked the tool, not against what you expect the tool to do.
- Stop editing and tell the user: what you were trying to achieve, what you tried, and the verbatim error. A blocked task reported early is worth more than another silent attempt.

If the oscillation is deliberate (a flag toggled on to test and off again), say so in one line and carry on.
</system-reminder>"#;

const ATTEMPT_REPEATED: &str = r#"<system-reminder>
You have just submitted the same change to {{path}} that you already submitted earlier in this turn ({{count}} attempts on this file so far). An identical retry gets an identical result.

If the first attempt was refused or failed, the reason still applies — read what it actually said. If you believe something has changed since, say what. Otherwise stop editing this file and report the blocker to the user.
</system-reminder>"#;

const FUTILE: &str = r#"<system-reminder>
Your last {{count}} attempts to modify {{path}} were all refused or failed, with nothing applied.

Stop and re-read the most recent error verbatim before trying again. If it is a refusal, the user has declined this action and repeating it in another form is not an alternative — find a different approach or tell them what you could not do. If it is an error, fix the cause you can name rather than varying the attempt.
</system-reminder>"#;

/// A rendered observation, ready to mount. A newtype rather than a `String`
/// so the only thing `ContextManager::set_progress_observation` can be handed
/// is text this module produced — the mount point is model-facing framing, not
/// a general-purpose injection channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered(String);

impl Rendered {
    pub fn into_text(self) -> String {
        self.0
    }
}

/// Render the observation for one verdict. `count` is the verdict's own
/// justification (edits made, attempts submitted, or refusals in a row).
pub fn render(kind: Kind, path: &str, count: usize) -> Rendered {
    let template = match kind {
        Kind::StateRevisited => STATE_REVISITED,
        Kind::AttemptRepeated => ATTEMPT_REPEATED,
        Kind::Futile => FUTILE,
    };
    Rendered(
        template
            .replace(PATH_PLACEHOLDER, path)
            .replace(COUNT_PLACEHOLDER, &count.to_string()),
    )
}

/// Which observation to render. Mirrors `baybo_agent`'s verdict enum without
/// `baybo-context` depending on `baybo-agent` (that edge would be a cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    StateRevisited,
    AttemptRepeated,
    Futile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_renders_both_placeholders() {
        for kind in [Kind::StateRevisited, Kind::AttemptRepeated, Kind::Futile] {
            let out = render(kind, "/tmp/x.json", 3).into_text();
            assert!(out.contains("/tmp/x.json"), "{kind:?}");
            assert!(out.contains('3'), "{kind:?}");
            assert!(!out.contains(PATH_PLACEHOLDER), "{kind:?}");
            assert!(!out.contains(COUNT_PLACEHOLDER), "{kind:?}");
            assert!(out.starts_with("<system-reminder>"), "{kind:?}");
        }
    }
}
