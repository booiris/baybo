//! Reasoning-effort allow-list per Codex model family + resolver.
//!
//! Codex Responses accepts a `reasoning.effort` field but each model
//! family advertises its own legal subset (e.g. `gpt-5-pro` only takes
//! `high`, `gpt-5.2+` adds `xhigh`, `gpt-codex` doesn't accept `none`
//! or `minimal`). Source of truth is openclaw's
//! `src/agents/openai-reasoning-effort.ts`.

/// Canonical effort levels used in the request body.
const NONE: &str = "none";
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

/// Resolve the effort the request body should use. Returns `None`
/// when reasoning is disabled (operator picked `none` and the model
/// supports it). Otherwise returns the canonical level.
///
/// `requested` is what the operator typed in `LlmEntry.reasoning_effort`
/// (case-insensitive, defaults applied when `None`); we clamp to the
/// model's allow-list, falling back to `default_effort` if the
/// requested value isn't permitted.
pub(crate) fn resolve_effort(model: &str, requested: Option<&str>) -> Option<&'static str> {
    let allowed = allowed_efforts(model);
    let pick = match requested.map(str::to_ascii_lowercase) {
        Some(level) => match level.as_str() {
            "none" => {
                if allowed.contains(&NONE) {
                    return None;
                }
                default_effort(model)
            }
            other => match allowed.iter().find(|e| e.eq_ignore_ascii_case(other)) {
                Some(e) => *e,
                None => default_effort(model),
            },
        },
        None => default_effort(model),
    };
    Some(pick)
}

/// Convenience for the CLI picker — list the effort levels available
/// for a given model, in display order.
pub fn allowed_efforts_for(model: &str) -> &'static [&'static str] {
    allowed_efforts(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt5_pro_only_accepts_high() {
        assert_eq!(allowed_efforts("gpt-5-pro"), &["high"]);
        assert_eq!(resolve_effort("gpt-5-pro", None), Some("high"));
        // Even when operator types something else, clamp.
        assert_eq!(resolve_effort("gpt-5-pro", Some("low")), Some("high"));
        assert_eq!(resolve_effort("gpt-5-pro", Some("none")), Some("high"));
    }

    #[test]
    fn gpt5_codex_supports_xhigh_but_not_minimal() {
        let efforts = allowed_efforts("gpt-codex");
        assert!(efforts.contains(&"xhigh"));
        assert!(!efforts.contains(&"minimal"));
        assert!(!efforts.contains(&"none"));
    }

    #[test]
    fn gpt5_minimal_is_legal() {
        let efforts = allowed_efforts("gpt-5");
        assert!(efforts.contains(&"minimal"));
        assert_eq!(resolve_effort("gpt-5", Some("minimal")), Some("minimal"));
    }

    #[test]
    fn none_is_only_honoured_when_model_supports_it() {
        // gpt-5.1 supports `none`, so `none` request → reasoning off.
        assert_eq!(resolve_effort("gpt-5.1", Some("none")), None);
        // gpt-5 does NOT support `none` — fall back to default.
        assert_eq!(resolve_effort("gpt-5", Some("none")), Some("medium"));
    }

    #[test]
    fn gpt5_2_unlocks_xhigh() {
        let efforts = allowed_efforts("gpt-5.2");
        assert!(efforts.contains(&"xhigh"));
        let efforts = allowed_efforts("gpt-5.3");
        assert!(efforts.contains(&"xhigh"));
    }

    #[test]
    fn unknown_model_falls_through_to_generic() {
        let efforts = allowed_efforts("brand-new-llm");
        assert_eq!(efforts, &["low", "medium", "high"]);
        assert_eq!(resolve_effort("brand-new-llm", None), Some("medium"));
    }

    #[test]
    fn invalid_request_falls_back_to_default() {
        assert_eq!(resolve_effort("gpt-5", Some("ultraplus")), Some("medium"));
    }
}
