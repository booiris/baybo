//! The unified diff both drift notices send their delta as.
//!
//! Shared because it is literally the same operation on both sides — a
//! `<system_prompt_update>` section body and a `<skills_update>` listing are
//! each line-oriented text whose old copy the conversation is still holding, so
//! the honest thing to send is the lines that moved. The two notices frame and
//! address their blocks differently; only this is common.

/// Lines of unchanged context around each hunk. Two is enough for the model to
/// locate the hunk in text it is already holding, and the anchor it actually
/// relies on is the `@@` line number.
const DIFF_CONTEXT_LINES: usize = 2;

/// Attribute marking a block whose body is a unified diff rather than a
/// replacement. One spelling, because each notice's framing explains it and
/// the two have to agree.
pub const DIFF_ATTR: &str = "diff";

/// A unified diff from `prior` to `current`, empty when they agree.
///
/// Each side is newline-terminated first, but only when it has content.
/// `from_lines` keeps the terminator as part of the line, so an unterminated
/// last line — which is every body, since the callers hand over trimmed text —
/// compares unequal to the same text once a line is appended after it, and one
/// appended memory would report the line before it as changed too. Terminating
/// an *empty* side, though, turns nothing into one empty line: emptying a file
/// rendered a phantom `+`, and filling an empty one rendered a `-` for a line
/// that never existed.
pub fn unified_diff(prior: &str, current: &str) -> String {
    let terminate = |s: &str| {
        if s.is_empty() {
            String::new()
        } else {
            format!("{s}\n")
        }
    };
    similar::TextDiff::from_lines(&terminate(prior), &terminate(current))
        .unified_diff()
        .context_radius(DIFF_CONTEXT_LINES)
        .missing_newline_hint(false)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty side is zero lines, not one blank one. Terminating it
    /// unconditionally made emptying a body report a phantom added blank line,
    /// and filling an empty one report a blank line removed.
    #[test]
    fn an_empty_side_contributes_no_line() {
        let emptied = unified_diff("- a: one\n- b: two", "");
        assert!(!emptied.lines().any(|l| l == "+"), "{emptied}");
        assert!(emptied.contains("-- a: one") && emptied.contains("-- b: two"));

        let filled = unified_diff("", "- a: one");
        assert!(!filled.lines().any(|l| l == "-"), "{filled}");
        assert!(filled.contains("+- a: one"), "{filled}");
    }

    /// The trap the termination exists for: an unterminated last line must not
    /// read as changed just because something was appended after it.
    #[test]
    fn an_appended_line_leaves_its_predecessor_as_context() {
        let out = unified_diff("one\ntwo", "one\ntwo\nthree");
        assert!(out.contains("+three"), "{out}");
        assert!(!out.contains("-two"), "{out}");
    }

    #[test]
    fn agreement_is_an_empty_diff() {
        assert!(unified_diff("same", "same").is_empty());
        assert!(unified_diff("", "").is_empty());
    }
}
