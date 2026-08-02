//! Reasoning-effort allow-list per Codex model family + resolver.
//!
//! Codex Responses accepts a `reasoning.effort` field but each model
//! family advertises its own legal subset (e.g. `gpt-5-pro` only takes
//! `high`, `gpt-5.2+` adds `xhigh`, `gpt-codex` doesn't accept `none`
//! or `minimal`). Source of truth is openclaw's
//! `src/agents/openai-reasoning-effort.ts`.

/// Canonical effort levels used in the request body.
pub(super) const NONE: &str = "none";
const MINIMAL: &str = "minimal";
const LOW: &str = "low";
const MEDIUM: &str = "medium";
const HIGH: &str = "high";
const XHIGH: &str = "xhigh";

/// Resolve the model id to the set of legal effort values, ordered
/// low → high so the resolver can fall through gracefully.
fn allowed_efforts(model: &str) -> &'static [&'static str] {
    let id = model.to_ascii_lowercase();
    // gpt-5-pro is the most restrictive — high only.
    if id == "gpt-5-pro" || id.starts_with("gpt-5-pro-") {
        return &[HIGH];
    }
    // gpt-pro: medium / high / xhigh.
    if id == "gpt-pro" || id.starts_with("gpt-pro-") {
        return &[MEDIUM, HIGH, XHIGH];
    }
    // gpt-codex / codex-mini: low / medium / high / xhigh.
    if id.contains("codex") {
        return &[LOW, MEDIUM, HIGH, XHIGH];
    }
    // gpt-5.2+: none / low / medium / high / xhigh.
    if let Some(rest) = id.strip_prefix("gpt-5.")
        && let Some(major_str) = rest.split(['-', '.']).next()
        && let Ok(major) = major_str.parse::<u32>()
        && major >= 2
    {
        return &[NONE, LOW, MEDIUM, HIGH, XHIGH];
    }
    // gpt-5.1: none / low / medium / high.
    if id == "gpt-5.1" || id.starts_with("gpt-5.1-") {
        return &[NONE, LOW, MEDIUM, HIGH];
    }
    // gpt-5: minimal / low / medium / high.
    if id == "gpt-5" || id.starts_with("gpt-5-") {
        return &[MINIMAL, LOW, MEDIUM, HIGH];
    }
    // Generic fallback for anything we don't recognise.
    &[LOW, MEDIUM, HIGH]
}

/// Default effort to pick when the operator hasn't specified one.
/// Returns the highest non-`none` value the model accepts that is
/// `<= medium` — matches Codex CLI's default tier.
fn default_effort(model: &str) -> &'static str {
    let allowed = allowed_efforts(model);
    if allowed.contains(&MEDIUM) {
        MEDIUM
    } else if allowed.contains(&LOW) {
        LOW
    } else {
        // gpt-5-pro etc. — fall back to whatever the only entry is.
        allowed.first().copied().unwrap_or(MEDIUM)
    }
}

/// Resolve the effort the request body should use. Returns `None` when
/// reasoning is off — the body then omits `reasoning` entirely.
///
/// `requested` is what the operator typed in `LlmEntry.reasoning_effort`
/// or pinned on the session, and it is passed through **verbatim**: a
/// level outside this model family's advertised set is sent as written
/// and refused by Codex, rather than being silently rewritten into a
/// level nobody asked for — which could just as easily raise effort
/// (`gpt-5-pro` accepts only `high`) as lower it. [`allowed_efforts_for`]
/// still backs the CLI picker, so the legal set stays discoverable up
/// front, where a wrong value is cheap to fix.
///
/// Only two values are interpreted rather than forwarded: `none` means
/// off, and an absent request falls back to the model family's default —
/// omitting the field would disable reasoning outright, which is not what
/// "the operator didn't specify" should mean.
pub(crate) fn resolve_effort(model: &str, requested: Option<&str>) -> Option<String> {
    match requested {
        Some(level) if level.eq_ignore_ascii_case(NONE) => None,
        Some(level) => Some(level.to_string()),
        None => Some(default_effort(model).to_string()),
    }
}

/// Convenience for the CLI picker — list the effort levels available
/// for a given model, in display order.
///
/// A display hint, not a gate: nothing validates a request against it, and
/// it is known to drift from what a given model actually takes. `gpt-5.6-sol`
/// answers `level 'max' not supported, valid levels: low, medium, high,
/// xhigh` — narrower than the `gpt-5.2+` row below, which also offers `none`.
/// The API is the authority; this table just keeps the picker close.
pub fn allowed_efforts_for(model: &str) -> &'static [&'static str] {
    allowed_efforts(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(model: &str, requested: Option<&str>) -> Option<String> {
        resolve_effort(model, requested)
    }

    /// The operator's level is sent as written. Rewriting it would be
    /// wrong in both directions: `gpt-5-pro` accepts only `high`, so
    /// clamping a `low` pick there silently bills the most expensive tier,
    /// while a level above a family's ceiling would silently drop.
    #[test]
    fn a_requested_level_is_never_rewritten() {
        assert_eq!(resolved("gpt-5-pro", Some("low")).as_deref(), Some("low"));
        assert_eq!(resolved("gpt-5", Some("xhigh")).as_deref(), Some("xhigh"));
        assert_eq!(
            resolved("gpt-5", Some("ultraplus")).as_deref(),
            Some("ultraplus"),
            "an unknown level goes to the provider, which is what reports it as invalid"
        );
    }

    /// `none` is the one level that means "omit the field", on every model.
    #[test]
    fn none_turns_reasoning_off_regardless_of_model() {
        assert_eq!(resolved("gpt-5.1", Some("none")), None);
        assert_eq!(resolved("gpt-5", Some("none")), None);
        assert_eq!(resolved("gpt-5-pro", Some("NONE")), None);
    }

    /// Absent ≠ off: omitting the field disables reasoning in the Codex
    /// body, so an unconfigured entry has to send the family default.
    #[test]
    fn an_unset_effort_falls_back_to_the_family_default() {
        assert_eq!(resolved("gpt-5-pro", None).as_deref(), Some("high"));
        assert_eq!(resolved("gpt-5", None).as_deref(), Some("medium"));
        assert_eq!(resolved("brand-new-llm", None).as_deref(), Some("medium"));
    }

    // The allow-lists no longer gate requests — they back the CLI picker,
    // so the levels it offers still have to be right.

    #[test]
    fn gpt5_pro_offers_only_high() {
        assert_eq!(allowed_efforts("gpt-5-pro"), &["high"]);
    }

    #[test]
    fn gpt5_codex_offers_xhigh_but_not_minimal() {
        let efforts = allowed_efforts("gpt-codex");
        assert!(efforts.contains(&"xhigh"));
        assert!(!efforts.contains(&"minimal"));
        assert!(!efforts.contains(&"none"));
    }

    #[test]
    fn gpt5_offers_minimal() {
        assert!(allowed_efforts("gpt-5").contains(&"minimal"));
    }

    #[test]
    fn gpt5_2_unlocks_xhigh() {
        assert!(allowed_efforts("gpt-5.2").contains(&"xhigh"));
        assert!(allowed_efforts("gpt-5.3").contains(&"xhigh"));
    }

    #[test]
    fn unknown_model_falls_through_to_generic() {
        assert_eq!(allowed_efforts("brand-new-llm"), &["low", "medium", "high"]);
    }
}
