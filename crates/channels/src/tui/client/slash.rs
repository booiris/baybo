//! [`SlashHandler`] backed by a [`GatewayClient`].
//!
//! Surfaces the subset of `/`-prefixed commands that have an HTTP
//! equivalent on the gateway. Anything CLI-only (`/workspace`,
//! `/doctor`, `/completion`, …) is hidden from completion and, when
//! typed, comes back as an `unsupported command` notice rather than
//! leaking through to the agent.
//!
//! The handler is intentionally shallow: it parses `/<name> <args>`
//! with `shell-words`, dispatches by name, and formats results as
//! plain text. Structured pretty-printing lives in `aura-cli`; bringing
//! it over would require that crate's formatter types (which we don't
//! want to drag into `aura-channels`).

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::ContentBlock;
use aura_tools::ApprovalDecision;

use crate::slash::{SlashCommand, SlashHandler, SlashOutcome, ViewKind};
use crate::tui::client::dto::ClientError;
use crate::tui::client::http::GatewayClient;

/// [`SlashHandler`] that dispatches supported commands against an
/// `aura gateway`.
pub struct GatewaySlashHandler {
    client: Arc<GatewayClient>,
    /// Skill names fetched at startup. Used to classify `/<name>` as a
    /// skill invocation (PassThrough to agent) rather than an unknown
    /// command. The list is a snapshot — reloading mid-session would
    /// require a live feed, which isn't exposed today.
    skills: Vec<String>,
}

impl GatewaySlashHandler {
    /// Construct from a shared client and a pre-fetched skill-name
    /// list. Callers are expected to have called
    /// [`GatewayClient::list_skills`] once at boot.
    pub fn new(client: Arc<GatewayClient>, skills: Vec<String>) -> Self {
        Self { client, skills }
    }

    fn is_skill_invocation(&self, first_token: &str) -> bool {
        self.skills.iter().any(|s| s == first_token)
    }
}

#[async_trait]
impl SlashHandler for GatewaySlashHandler {
    fn commands(&self) -> Vec<SlashCommand> {
        let mut out = vec![
            SlashCommand::new("/sessions", "List sessions on the gateway."),
            SlashCommand::new("/jobs", "List jobs on the gateway."),
            SlashCommand::new("/skills", "List registered skills."),
            SlashCommand::new("/tools", "List registered tools."),
            SlashCommand::new("/channels", "List active channels."),
            SlashCommand::new("/memory", "List memory entries."),
            SlashCommand::new("/status", "Show gateway status."),
            SlashCommand::new("/llm", "Show LLM provider info."),
            SlashCommand::new("/trace", "Fetch a trace by id (/trace <id>)."),
            SlashCommand::new("/config", "Get or mutate gateway config."),
            SlashCommand::new("/approve", "Approve a pending tool call (/approve <id>)."),
            SlashCommand::new("/deny", "Deny a pending tool call (/deny <id>)."),
            SlashCommand::new("/clear", "Clear the chat scrollback."),
            SlashCommand::new("/quit", "Exit the chat session."),
            SlashCommand::new("/exit", "Exit the chat session."),
        ];
        for skill in &self.skills {
            out.push(SlashCommand::new(format!("/{skill}"), "skill invocation"));
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn handle(&self, raw: &str) -> SlashOutcome {
        let Some((name, args)) = parse_line(raw) else {
            return SlashOutcome::PassThrough;
        };

        // Bare dashboard commands → open view.
        if args.is_empty()
            && let Some(kind) = dashboard_shortcut(&name)
        {
            return SlashOutcome::OpenView(kind);
        }

        // Known skill name → let the agent handle it.
        if self.is_skill_invocation(&name) {
            return SlashOutcome::PassThrough;
        }

        match name.as_str() {
            "clear" => SlashOutcome::Handled(Vec::new()),
            "quit" | "exit" => SlashOutcome::Exit,
            "status" => run(self.client.status().await.map(format_status)),
            "llm" => run(self.client.get_llm().await.map(|v| format_json("llm", &v))),
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
            "jobs" => run(self.client.list_jobs().await.map(|items| {
                let lines: Vec<String> = items
                    .iter()
                    .map(|j| {
                        format!(
                            "{}  status={}  session={}  created={}",
                            j.id,
                            j.status,
                            j.session_id,
                            j.created_at.to_rfc3339()
                        )
                    })
                    .collect();
                list_block("jobs", lines)
            })),
            "skills" => run(self
                .client
                .list_skills()
                .await
                .map(|items| list_block("skills", items))),
            "tools" => run(self
                .client
                .list_tools()
                .await
                .map(|items| list_json("tools", &items))),
            "channels" => run(self
                .client
                .list_channels()
                .await
                .map(|items| list_json("channels", &items))),
            "memory" => run(self.client.list_memory().await.map(|items| {
                let lines: Vec<String> = items
                    .iter()
                    .map(|m| {
                        format!(
                            "{}  user={}  category={}  imp={:.2}  {}",
                            m.id, m.user_id, m.category, m.importance, m.content
                        )
                    })
                    .collect();
                list_block("memory", lines)
            })),
            "trace" => match args.first() {
                Some(id) => run(self
                    .client
                    .get_trace(id)
                    .await
                    .map(|v| format_json(&format!("trace {id}"), &v))),
                None => err("usage: /trace <id>"),
            },
            "config" => handle_config(&self.client, &args).await,
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
            "help" => SlashOutcome::Handled(vec![ContentBlock::Text(help_text(&self.skills))]),
            _ => err(&format!(
                "unknown command: /{name}\nrun /help to see supported commands"
            )),
        }
    }
}

/// Bare `/skills`, `/jobs`, `/sessions`, `/memory` with no args open the
/// corresponding dashboard view.
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

async fn handle_config(client: &GatewayClient, args: &[String]) -> SlashOutcome {
    match args.first().map(String::as_str) {
        None | Some("get") => {
            let path = args.get(1);
            match client.get_config().await {
                Ok(cfg) => {
                    let rendered = match path {
                        Some(p) => match pointer_get(&cfg, p) {
                            Some(v) => format_json(p, &v),
                            None => format!("config: no value at `{p}`"),
                        },
                        None => format_json("config", &cfg),
                    };
                    SlashOutcome::Handled(vec![ContentBlock::Text(rendered)])
                }
                Err(e) => err(&e.to_string()),
            }
        }
        Some("set") => {
            let Some(path) = args.get(1) else {
                return err("usage: /config set <path> <value>");
            };
            let Some(value_str) = args.get(2) else {
                return err("usage: /config set <path> <value>");
            };
            let value = serde_json::from_str::<serde_json::Value>(value_str)
                .unwrap_or_else(|_| serde_json::Value::String(value_str.clone()));
            match client.set_config(path, value).await {
                Ok(r) => SlashOutcome::Handled(vec![ContentBlock::Text(format!(
                    "config: wrote {} to {} (restart gateway to apply)",
                    r.path, r.written_to
                ))]),
                Err(e) => err(&e.to_string()),
            }
        }
        Some("unset") => {
            let Some(path) = args.get(1) else {
                return err("usage: /config unset <path>");
            };
            match client.unset_config(path).await {
                Ok(r) => SlashOutcome::Handled(vec![ContentBlock::Text(format!(
                    "config: removed {} from {} (restart gateway to apply)",
                    r.path, r.written_to
                ))]),
                Err(e) => err(&e.to_string()),
            }
        }
        Some(other) => err(&format!(
            "unknown subcommand: config {other}\nsupported: get, set, unset"
        )),
    }
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

fn format_status(s: crate::tui::client::dto::StatusResponse) -> String {
    format!(
        "gateway {version}\n  bind={bind}\n  sessions={sessions}\n  jobs_in_flight={jobs}",
        version = s.version,
        bind = s.bind_address,
        sessions = s.sessions,
        jobs = s.jobs_in_flight
    )
}

fn format_json(title: &str, value: &serde_json::Value) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(s) => format!("{title}:\n{s}"),
        Err(_) => format!("{title}: <unrenderable>"),
    }
}

fn list_json(title: &str, items: &[serde_json::Value]) -> String {
    if items.is_empty() {
        return format!("no {title}");
    }
    let body: Vec<String> = items
        .iter()
        .filter_map(|v| serde_json::to_string(v).ok())
        .collect();
    format!("{title}:\n{}", body.join("\n"))
}

fn list_block(title: &str, lines: Vec<String>) -> String {
    if lines.is_empty() {
        return format!("no {title}");
    }
    format!("{title}:\n{}", lines.join("\n"))
}

/// Simple `a.b.c` → value lookup on a JSON config.
fn pointer_get(root: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut cur = root;
    for segment in path.split('.') {
        cur = cur.get(segment)?;
    }
    Some(cur.clone())
}

fn help_text(skills: &[String]) -> String {
    let mut out = String::from(
        "Slash commands:\n\
         /sessions        list sessions\n\
         /jobs            list jobs\n\
         /skills          list skills\n\
         /tools           list tools\n\
         /channels        list channels\n\
         /memory          list memory entries\n\
         /status          gateway status\n\
         /llm             LLM provider info\n\
         /trace <id>      fetch a trace\n\
         /config [get|set|unset] [path] [value]\n\
         /approve <id>    approve a pending tool call\n\
         /deny <id>       deny a pending tool call\n\
         /clear           clear chat scrollback\n\
         /quit, /exit     close the session\n",
    );
    if !skills.is_empty() {
        out.push_str("\nskills available as /<name>:\n  ");
        out.push_str(&skills.join(", "));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_extracts_name_and_args() {
        let (name, args) = parse_line("/config set foo bar").unwrap();
        assert_eq!(name, "config");
        assert_eq!(args, vec!["set", "foo", "bar"]);
    }

    #[test]
    fn parse_line_honours_quoting() {
        let (_, args) = parse_line(r#"/config set path "a b c""#).unwrap();
        assert_eq!(args, vec!["set", "path", "a b c"]);
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

    #[test]
    fn pointer_get_walks_nested() {
        let root = serde_json::json!({ "a": { "b": { "c": 42 } } });
        assert_eq!(pointer_get(&root, "a.b.c"), Some(serde_json::json!(42)));
        assert_eq!(
            pointer_get(&root, "a.b"),
            Some(serde_json::json!({"c": 42}))
        );
        assert_eq!(pointer_get(&root, "a.x"), None);
    }
}
