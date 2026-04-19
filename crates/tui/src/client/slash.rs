//! [`SlashHandler`] backed by a [`GatewayClient`].
//!
//! The TUI only reaches the gateway's channel surface (sessions +
//! approvals), so slash commands here are limited to what that surface
//! can serve: `/sessions` lists, `/approve` and `/deny` resolve tool
//! approvals, plus a handful of client-local commands (`/clear`,
//! `/quit`, `/exit`). Admin-only commands (`/status`, `/config`, …)
//! live in `aura cli`.
//!
//! The handler is intentionally shallow: it parses `/<name> <args>`
//! with `shell-words`, dispatches by name, and formats results as
//! plain text. Unknown `/<name>` is passed through to the agent so
//! skill invocations continue to work without the gateway having to
//! hand the TUI a skill allow-list.

use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::{SlashCommand, SlashHandler, SlashOutcome, ViewKind};
use aura_model::ContentBlock;
use aura_tools::ApprovalDecision;

use crate::client::dto::ClientError;
use crate::client::http::GatewayClient;

/// [`SlashHandler`] that dispatches supported commands against an
/// `aura gateway`.
pub struct GatewaySlashHandler {
    client: Arc<GatewayClient>,
}

impl GatewaySlashHandler {
    pub fn new(client: Arc<GatewayClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl SlashHandler for GatewaySlashHandler {
    fn commands(&self) -> Vec<SlashCommand> {
        let mut out = vec![
            SlashCommand::new("/sessions", "List sessions on the gateway."),
            SlashCommand::new("/approve", "Approve a pending tool call (/approve <id>)."),
            SlashCommand::new("/deny", "Deny a pending tool call (/deny <id>)."),
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

        // Bare dashboard commands → open view. `Sessions` is the only
        // channel-surface view; the others render an admin-only footer
        // when opened.
        if args.is_empty()
            && let Some(kind) = dashboard_shortcut(&name)
        {
            return SlashOutcome::OpenView(kind);
        }

        match name.as_str() {
            "clear" => SlashOutcome::Handled(Vec::new()),
            "quit" | "exit" => SlashOutcome::Exit,
            "sessions" => run(self.client.list_sessions().await.map(|items| {
                let lines: Vec<String> = items
                    .iter()
                    .map(|s| {
                        format!(
                            "{}  {:>4} msgs  last={}  channel={}",
                            s.id,
                            s.messages.len(),
                            s.last_active.to_rfc3339(),
                            s.channel
                        )
                    })
                    .collect();
                list_block("sessions", lines)
            })),
            "approve" | "deny" => {
                let Some(id) = args.first() else {
                    return err(&format!("usage: /{name} <call_id>"));
                };
                let decision = if name == "approve" {
                    ApprovalDecision::Approve
                } else {
                    ApprovalDecision::Deny
                };
                run(self
                    .client
                    .resolve_approval(id, decision)
                    .await
                    .map(|r| format!("resolved {} as {:?}", r.call_id, r.decision)))
            }
            "help" => SlashOutcome::Handled(vec![ContentBlock::Text(help_text())]),
            // Anything else — unknown slash — falls through to the
            // agent so skill invocations keep working without a
            // client-side allow-list.
            _ => SlashOutcome::PassThrough,
        }
    }
}

/// Bare `/sessions` / `/skills` / `/jobs` / `/memory` with no args open
/// the corresponding dashboard view. The admin-only views render a
/// footer telling the user to use `aura cli`, but the TUI still honours
/// the shortcut so keybindings stay consistent with the CLI surface.
fn dashboard_shortcut(name: &str) -> Option<ViewKind> {
    match name {
        "skills" => Some(ViewKind::Skills),
        "jobs" => Some(ViewKind::Jobs),
        "sessions" => Some(ViewKind::Sessions),
        "memory" => Some(ViewKind::Memory),
        _ => None,
    }
}

/// Split `/name arg1 arg2` into `(name, [arg1, arg2])`. Returns `None`
/// if the line isn't a slash command (no leading `/`, empty name, or
/// unparseable quoting).
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

fn run(result: Result<String, ClientError>) -> SlashOutcome {
    match result {
        Ok(text) => SlashOutcome::Handled(vec![ContentBlock::Text(text)]),
        Err(e) => err(&e.to_string()),
    }
}

fn err(msg: &str) -> SlashOutcome {
    SlashOutcome::Handled(vec![ContentBlock::Text(format!("error: {msg}"))])
}

fn list_block(title: &str, lines: Vec<String>) -> String {
    if lines.is_empty() {
        return format!("no {title}");
    }
    format!("{title}:\n{}", lines.join("\n"))
}

fn help_text() -> String {
    String::from(
        "Slash commands:\n\
         /sessions        list sessions\n\
         /approve <id>    approve a pending tool call\n\
         /deny <id>       deny a pending tool call\n\
         /clear           clear chat scrollback\n\
         /quit, /exit     close the session\n\
         \n\
         Admin commands (status, config, jobs, skills, tools, memory, …)\n\
         live in `aura cli` and are not reachable from the TUI.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_extracts_name_and_args() {
        let (name, args) = parse_line("/approve deadbeef").unwrap();
        assert_eq!(name, "approve");
        assert_eq!(args, vec!["deadbeef"]);
    }

    #[test]
    fn parse_line_honours_quoting() {
        let (_, args) = parse_line(r#"/approve "a b c""#).unwrap();
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
        assert_eq!(dashboard_shortcut("memory"), Some(ViewKind::Memory));
        assert_eq!(dashboard_shortcut("status"), None);
    }
}
