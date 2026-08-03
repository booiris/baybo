//! The roster of tools a session could load but has not.
//!
//! A deferred tool costs nothing until asked for, which only works if the
//! model knows it exists. That is this: a list of bare names, injected as a
//! `<system-reminder>` message.
//!
//! **Names, not descriptions.** A name like `browser/read_page` already says
//! what the tool is for, and a description per entry would cost most of what
//! deferring saved. The one line of framing below carries what a name cannot:
//! how to load, and what happens if you skip that step.
//!
//! **A message, never the tool array.** The array is the front of the
//! provider's cache prefix, so a change there invalidates the system prompt
//! and the whole transcript behind it. Messages are append-only, so the roster
//! and every later delta are free of that. The same reasoning is why
//! `messages_for_llm` carries superseded `SystemPromptUpdate` rows rather than
//! dropping them.

/// Opening of the roster reminder.
const ROSTER_HEAD: &str = r#"These tools exist but are NOT in your tool list, because carrying a tool costs you on every message whether or not you ever call it. Load one with `ToolSearch` — `select:<name>` for a specific tool (name several at once: `select:A,B,C`), or a plain phrase to search by capability. Calling one before loading it fails.

Nothing else advertises these, so a name that is not listed here does not exist:"#;

/// Opening of a delta announcing tools that appeared mid-conversation.
const ARRIVED_HEAD: &str = r#"These tools just became loadable with `ToolSearch`, in addition to the ones listed earlier:"#;

/// Opening of a delta announcing tools that went away mid-conversation.
///
/// Withdrawal is announced rather than silently applied so a model that was
/// about to reach for one is told why it cannot, instead of getting a bare
/// failure from `ToolSearch`.
const WITHDRAWN_HEAD: &str = r#"These tools are no longer available (whatever provided them went away). Do not try to load them; `ToolSearch` will not find them:"#;

fn wrap(head: &str, names: &[String]) -> String {
    let mut out = String::from("<system-reminder>\n");
    out.push_str(head);
    out.push('\n');
    for name in names {
        out.push_str("- ");
        out.push_str(name);
        out.push('\n');
    }
    out.push_str("</system-reminder>");
    out
}

/// The full roster, or `None` when nothing is deferred — a reminder listing
/// nothing would spend tokens teaching a mechanism this session cannot use.
pub fn render_roster(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| wrap(ROSTER_HEAD, names))
}

/// What changed since the session was last told, or `None` when nothing did.
///
/// Both directions land in one message: a sidecar that restarts under a new
/// identity can withdraw and re-offer in the same breath, and splitting that
/// into two reminders would read as two unrelated events.
pub fn render_delta(arrived: &[String], withdrawn: &[String]) -> Option<String> {
    if arrived.is_empty() && withdrawn.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !arrived.is_empty() {
        parts.push(wrap(ARRIVED_HEAD, arrived));
    }
    if !withdrawn.is_empty() {
        parts.push(wrap(WITHDRAWN_HEAD, withdrawn));
    }
    Some(parts.join("\n\n"))
}

/// `(arrived, withdrawn)` between what the session was told and what is
/// loadable now. Both inputs are sorted, so this is a linear merge and the
/// output is sorted too — a delta message is byte-stable for a given change.
pub fn diff(previous: &[String], current: &[String]) -> (Vec<String>, Vec<String>) {
    let arrived = current
        .iter()
        .filter(|n| !previous.contains(n))
        .cloned()
        .collect();
    let withdrawn = previous
        .iter()
        .filter(|n| !current.contains(n))
        .cloned()
        .collect();
    (arrived, withdrawn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_roster_lists_every_name() {
        let out = render_roster(&names(&["browser/click", "CronCreate"])).expect("roster");
        assert!(out.contains("- browser/click"), "{out}");
        assert!(out.contains("- CronCreate"), "{out}");
        assert!(out.starts_with("<system-reminder>"), "{out}");
        assert!(out.ends_with("</system-reminder>"), "{out}");
    }

    /// The roster is the only place these names appear, so it has to say how
    /// to act on them — a bare list would read as trivia.
    #[test]
    fn the_roster_says_how_to_load_and_what_happens_if_you_do_not() {
        let out = render_roster(&names(&["CronCreate"])).expect("roster");
        assert!(out.contains("ToolSearch"), "{out}");
        assert!(out.contains("select:"), "{out}");
        assert!(out.contains("fails"), "{out}");
    }

    #[test]
    fn nothing_deferred_means_no_reminder() {
        assert!(render_roster(&[]).is_none());
    }

    #[test]
    fn a_delta_reports_both_directions_in_one_message() {
        let out = render_delta(&names(&["browser/click"]), &names(&["mcp/old"])).expect("delta");
        assert!(out.contains("- browser/click"), "{out}");
        assert!(out.contains("- mcp/old"), "{out}");
        assert!(out.contains("no longer available"), "{out}");
    }

    #[test]
    fn no_change_means_no_delta() {
        assert!(render_delta(&[], &[]).is_none());
    }

    #[test]
    fn diff_reports_each_side() {
        let (arrived, withdrawn) = diff(&names(&["a", "b"]), &names(&["b", "c"]));
        assert_eq!(arrived, names(&["c"]));
        assert_eq!(withdrawn, names(&["a"]));
    }

    #[test]
    fn an_unchanged_set_diffs_to_nothing() {
        let (arrived, withdrawn) = diff(&names(&["a", "b"]), &names(&["a", "b"]));
        assert!(arrived.is_empty() && withdrawn.is_empty());
    }
}
