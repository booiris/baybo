use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::{SlashCommand, SlashHandler, SlashOutcome, ViewKind};
use aura_model::ContentBlock;
use clap::{CommandFactory, Parser};

use crate::cli::Cli;
use crate::context::{CommandContext, Invocation};
use crate::dispatch;
use crate::format::{CommandOutput, OutputFormat};

/// Routes `/`-prefixed chat input through the same clap tree used for argv.
///
/// Shared state (`CommandContext`) is taken by `Arc`. Each `/command`
/// invocation clones the context, flips it into slash mode, and dispatches.
pub struct CliSlashHandler {
    ctx: Arc<CommandContext>,
}

impl CliSlashHandler {
    pub fn new(ctx: Arc<CommandContext>) -> Self {
        Self { ctx }
    }

    async fn try_dispatch(&self, raw: &str) -> Result<CommandOutput, DispatchError> {
        let trimmed = raw.trim();
        let without_slash = trimmed.strip_prefix('/').unwrap_or(trimmed);
        if without_slash.is_empty() {
            return Err(DispatchError::NotACommand);
        }
        let tokens = shell_words::split(without_slash)
            .map_err(|e| DispatchError::Parse(format!("split: {e}")))?;
        if tokens.is_empty() {
            return Err(DispatchError::NotACommand);
        }
        // `try_parse_from` expects argv[0] to be the program name.
        let mut argv: Vec<String> = Vec::with_capacity(tokens.len() + 1);
        argv.push("aura".to_string());
        argv.extend(tokens);
        let cli = Cli::try_parse_from(&argv).map_err(|e| DispatchError::Parse(e.to_string()))?;
        let cmd = cli.command.ok_or(DispatchError::NotACommand)?;

        // Slash mode: force Plain unless caller asked for JSON.
        let format = if cli.global.json {
            OutputFormat::Json
        } else {
            OutputFormat::Plain
        };
        let slash_ctx = CommandContext {
            config: self.ctx.config.clone(),
            config_path: self.ctx.config_path.clone(),
            skills: self.ctx.skills.clone(),
            tools: self.ctx.tools.clone(),
            channels: self.ctx.channels.clone(),
            llm: self.ctx.llm.clone(),
            workspace: self.ctx.workspace.clone(),
            session: self.ctx.session.clone(),
            job: self.ctx.job.clone(),
            cron: self.ctx.cron.clone(),
            memory: self.ctx.memory.clone(),
            trace: self.ctx.trace.clone(),
            tool_executor: self.ctx.tool_executor.clone(),
            recorder: self.ctx.recorder.clone(),
            security: self.ctx.security.clone(),
            leak_detector: self.ctx.leak_detector.clone(),
            format,
            invocation: Invocation::Slash,
            confirmed: false,
        };
        dispatch::run(&slash_ctx, cmd)
            .await
            .map_err(DispatchError::Cli)
    }
}

#[async_trait]
impl SlashHandler for CliSlashHandler {
    fn commands(&self) -> Vec<SlashCommand> {
        let mut out = Vec::new();
        let cmd = Cli::command();
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() {
                continue;
            }
            let name = sub.get_name();
            if name == "help" || name == "completion" {
                continue;
            }
            let about = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
            out.push(SlashCommand::new(format!("/{name}"), about));
        }
        // TUI-adapter built-ins that never reach the clap tree.
        out.push(SlashCommand::new("/clear", "Clear the chat scrollback."));
        out.push(SlashCommand::new("/quit", "Exit the chat session."));
        out.push(SlashCommand::new("/exit", "Exit the chat session."));
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn handle(&self, raw: &str) -> SlashOutcome {
        // Bare `/skills`, `/tools`, `/jobs`, `/sessions`, `/memory` (no args)
        // open the corresponding dashboard view in TUI-capable adapters;
        // adapters that don't support views treat this as a no-op.
        if let Some(kind) = dashboard_shortcut(raw) {
            return SlashOutcome::OpenView(kind);
        }
        match self.try_dispatch(raw).await {
            Ok(out) => {
                let format = if out.data.is_some() && raw.contains("--json") {
                    OutputFormat::Json
                } else {
                    OutputFormat::Plain
                };
                SlashOutcome::Handled(vec![ContentBlock::Text(out.render(format))])
            }
            Err(DispatchError::NotACommand) => SlashOutcome::PassThrough,
            Err(DispatchError::Parse(msg)) => {
                SlashOutcome::Handled(vec![ContentBlock::Text(format!("command error:\n{msg}"))])
            }
            Err(DispatchError::Cli(e)) => {
                SlashOutcome::Handled(vec![ContentBlock::Text(format!("error: {e}"))])
            }
        }
    }
}

/// Match bare dashboard commands (`/skills`, `/tools`, ...) with no arguments.
/// Anything with arguments or unknown names falls through to the clap path.
fn dashboard_shortcut(raw: &str) -> Option<ViewKind> {
    let without_slash = raw.trim().strip_prefix('/')?;
    let tokens = shell_words::split(without_slash).ok()?;
    let [cmd] = tokens.as_slice() else {
        return None;
    };
    match cmd.as_str() {
        "skills" => Some(ViewKind::Skills),
        "tools" => Some(ViewKind::Tools),
        "jobs" => Some(ViewKind::Jobs),
        "sessions" => Some(ViewKind::Sessions),
        "memory" => Some(ViewKind::Memory),
        _ => None,
    }
}

enum DispatchError {
    NotACommand,
    Parse(String),
    Cli(crate::error::CliError),
}
