use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::{SlashHandler, SlashOutcome};
use aura_model::ContentBlock;
use clap::Parser;

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
    async fn handle(&self, raw: &str) -> SlashOutcome {
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

enum DispatchError {
    NotACommand,
    Parse(String),
    Cli(crate::error::CliError),
}
