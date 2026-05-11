//! Minimal [`SlashHandler`] for the WS-backed TUI.
//!
//! The WS channel surface carries messages and approval frames but no
//! admin-style CRUD — there is no `/sessions` listing and no channel
//! REST. This handler therefore ships only:
//!
//! * `/clear` — clear the chat scrollback (client-local).
//! * `/quit`, `/exit` — close the TUI.
//! * `/skills`, `/jobs`, `/memory`, `/sessions` — dashboard shortcuts;
//!   the admin-only views render an "admin surface" footer.
//!
//! Tool approvals are resolved through the modal keybindings (a/A/d);
//! there is no slash-command surface for them.
//!
//! Unknown `/<name>` falls through to the agent so skill invocations
//! keep working without a client-side allow-list.

use async_trait::async_trait;
use aura_channels::{SlashCommand, SlashHandler, SlashOutcome, ViewKind};
use aura_model::ContentBlock;

/// Bundled slash handler used by the TUI.
pub struct TuiSlashHandler;

impl TuiSlashHandler {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TuiSlashHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SlashHandler for TuiSlashHandler {
    fn commands(&self) -> Vec<SlashCommand> {
        let mut out = vec![
            SlashCommand::new("/clear", "Clear the chat scrollback."),
            SlashCommand::new("/quit", "Exit the chat session."),
            SlashCommand::new("/exit", "Exit the chat session."),
        ];
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn handle(&self, raw: &str) -> SlashOutcome {
        let Some((name, args)) = parse_line(raw) else {
            return SlashOutcome::PassThrough;
        };

        if args.is_empty()
            && let Some(kind) = dashboard_shortcut(&name)
        {
            return SlashOutcome::OpenView(kind);
        }

        match name.as_str() {
            "clear" => SlashOutcome::Handled(Vec::new()),
            "quit" | "exit" => SlashOutcome::Exit,
            "help" => SlashOutcome::Handled(vec![ContentBlock::Text(help_text())]),
            _ => SlashOutcome::PassThrough,
        }
    }
}

fn dashboard_shortcut(name: &str) -> Option<ViewKind> {
    match name {
        "skills" => Some(ViewKind::Skills),
        "jobs" => Some(ViewKind::Jobs),
        "sessions" => Some(ViewKind::Sessions),
        _ => None,
    }
}

fn parse_line(raw: &str) -> Option<(String, Vec<String>)> {
    let trimmed = raw.trim().strip_prefix('/')?;
    let tokens = shell_words::split(trimmed).ok()?;
    let mut iter = tokens.into_iter();
    let name = iter.next()?;
    if name.is_empty() {
        return None;
    }
    Some((name, iter.collect()))
}

fn help_text() -> String {
    String::from(
        "Slash commands:\n\
         /clear           clear chat scrollback\n\
         /quit, /exit     close the session\n\
         \n\
         Tool approvals are resolved from the modal (a / A / d).\n\
         \n\
         Admin commands (status, config, jobs, skills, tools,\n\
         sessions, …) are reached via the `aura` CLI and are not\n\
         reachable from the TUI.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_extracts_name_and_args() {
        let (name, args) = parse_line("/foo deadbeef").unwrap();
        assert_eq!(name, "foo");
        assert_eq!(args, vec!["deadbeef"]);
    }

    #[test]
    fn parse_line_honours_quoting() {
        let (_, args) = parse_line(r#"/foo "a b c""#).unwrap();
        assert_eq!(args, vec!["a b c"]);
    }

    #[test]
    fn parse_line_rejects_non_slash() {
        assert!(parse_line("hello").is_none());
    }

    #[test]
    fn parse_line_rejects_empty_name() {
        assert!(parse_line("/").is_none());
    }

    #[test]
    fn dashboard_shortcut_matches_known_views() {
        assert_eq!(dashboard_shortcut("skills"), Some(ViewKind::Skills));
        assert_eq!(dashboard_shortcut("jobs"), Some(ViewKind::Jobs));
        assert_eq!(dashboard_shortcut("sessions"), Some(ViewKind::Sessions));
        // `memory` retired with the CLI `aura memory` family.
        assert_eq!(dashboard_shortcut("memory"), None);
        assert_eq!(dashboard_shortcut("status"), None);
    }
}
