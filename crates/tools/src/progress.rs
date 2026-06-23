//! Shared helpers for [`crate::Tool::progress_label`] previews — the short
//! `● Tool(<label>)` text shown in the live turn-progress line (see
//! `docs/turn-progress-events.md`). Kept in one place so every tool, builtin
//! or domain-crate, renders paths and arguments the same width-capped way;
//! the TUI renderer does not truncate, so the cap lives here.

use std::path::Path;

/// Max display width (in chars) for a progress label, so a long path or
/// command doesn't blow out the `● Tool(label)` line.
pub const PROGRESS_LABEL_MAX: usize = 60;

/// One-line preview of a filesystem path: the full path when it fits,
/// otherwise left-truncated to keep the file name (and as many trailing
/// parent components as fit) behind a leading `…`. Truncation snaps to a
/// `/` boundary so a partial directory name never shows.
pub fn preview_path(path: &Path) -> String {
    let full = path.to_string_lossy();
    if full.chars().count() <= PROGRESS_LABEL_MAX {
        return full.into_owned();
    }
    // Keep the trailing chars, reserving 2 for the leading "…/", then drop
    // any partial component before the first '/' so the head reads cleanly.
    let budget = PROGRESS_LABEL_MAX - 2;
    let skip = full.chars().count() - budget;
    let tail: String = full.chars().skip(skip).collect();
    match tail.find('/') {
        Some(idx) => format!("…/{}", &tail[idx + 1..]),
        None => format!("…{tail}"),
    }
}

/// One-line preview of a free-form argument (a command, a search pattern, a
/// task summary): inner whitespace collapsed to single spaces, then
/// right-truncated with a trailing `…`. `None` when the argument is empty.
pub fn preview_arg(s: &str) -> Option<String> {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return None;
    }
    if one_line.chars().count() > PROGRESS_LABEL_MAX {
        Some(format!(
            "{}…",
            one_line
                .chars()
                .take(PROGRESS_LABEL_MAX)
                .collect::<String>()
        ))
    } else {
        Some(one_line)
    }
}

/// `<pattern> · in <path>` preview for search tools (Grep/Glob): the search
/// pattern ([`preview_arg`]-normalised) optionally suffixed with the search
/// root, so the live line reads e.g. `● Grep(TODO · in …/crates/tui)`.
pub fn preview_search(pattern: &str, path: Option<&str>) -> Option<String> {
    let pattern = preview_arg(pattern)?;
    match path.filter(|p| !p.is_empty()) {
        Some(p) => Some(format!("{pattern} · in {}", preview_path(Path::new(p)))),
        None => Some(pattern),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_path_keeps_short_path_intact() {
        let p = Path::new("/data/baybo/crates/tools/src/builtin/read.rs");
        assert_eq!(
            preview_path(p),
            "/data/baybo/crates/tools/src/builtin/read.rs"
        );
    }

    #[test]
    fn preview_path_left_truncates_long_path_on_separator() {
        let p = Path::new(
            "/data/baybo/some/really/deeply/nested/workspace/crates/agent/src/runtime/agent_loop.rs",
        );
        let out = preview_path(p);
        assert!(out.starts_with('…'), "leads with ellipsis: {out}");
        assert!(out.ends_with("agent_loop.rs"), "keeps the file name: {out}");
        assert!(
            out.chars().count() <= PROGRESS_LABEL_MAX,
            "within cap: {out}"
        );
        // The head after "…/" is a clean component, not a sliced dir name.
        assert!(out.starts_with("…/"), "snaps to a separator: {out}");
    }

    #[test]
    fn preview_path_handles_overlong_file_name_without_separator() {
        let name = "x".repeat(120);
        let p = Path::new(&name);
        let out = preview_path(p);
        assert!(out.starts_with('…'));
        assert!(out.chars().count() <= PROGRESS_LABEL_MAX);
    }

    #[test]
    fn preview_arg_collapses_whitespace() {
        assert_eq!(
            preview_arg("echo a\n   echo b").as_deref(),
            Some("echo a echo b")
        );
    }

    #[test]
    fn preview_arg_caps_length_with_ellipsis() {
        let out = preview_arg(&"x".repeat(200)).expect("non-empty");
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), PROGRESS_LABEL_MAX + 1);
    }

    #[test]
    fn preview_arg_empty_is_none() {
        assert_eq!(preview_arg("   \n  "), None);
    }

    #[test]
    fn preview_search_appends_path_when_present() {
        let out = preview_search("TODO", Some("/data/baybo/crates/tui")).unwrap();
        assert_eq!(out, "TODO · in /data/baybo/crates/tui");
    }

    #[test]
    fn preview_search_pattern_only_without_path() {
        assert_eq!(preview_search("TODO", None).as_deref(), Some("TODO"));
        assert_eq!(preview_search("TODO", Some("")).as_deref(), Some("TODO"));
    }
}
