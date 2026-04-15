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
            skill_assessor: self.ctx.skill_assessor.clone(),
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
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
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
            let slash = format!("/{name}");
            seen.insert(slash.clone());
            out.push(SlashCommand::new(slash, about));
        }
        // User-invocable skills surface as slash commands alongside built-ins.
        // Clap subcommands take precedence on name collisions so operators
        // can't shadow `/config` or `/skills` with a workspace skill.
        for skill in self.ctx.skills.all_sorted() {
            let Some(cmd_name) = skill.command.as_deref() else {
                continue;
            };
            let slash = format!("/{cmd_name}");
            if !seen.insert(slash.clone()) {
                continue;
            }
            let hint = match &skill.argument_hint {
                Some(h) if !h.is_empty() => format!("{h}  {}", skill.description),
                _ => skill.description.clone(),
            };
            out.push(SlashCommand::new(slash, hint));
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
        // Skill slash commands (`/<skill>` with optional args) aren't in the
        // clap tree — forward them to the agent so `SkillRegistry::select`
        // can narrow on the exact-match branch. Only do this when the first
        // token actually names a user-invocable skill; otherwise fall through
        // to clap, which yields the normal "unknown command" error.
        if is_skill_invocation(raw, &self.ctx.skills) {
            return SlashOutcome::PassThrough;
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

/// Return true when the first whitespace-separated token after the leading
/// `/` names a user-invocable skill. Skill lookup goes through `get`, which
/// indexes by skill name; we also require the stored `command` to match so
/// skills with `user-invocable: false` are ignored.
fn is_skill_invocation(raw: &str, skills: &aura_skills::SkillRegistry) -> bool {
    let without_slash = raw.trim().strip_prefix('/').unwrap_or("");
    let Some(first) = without_slash.split_whitespace().next() else {
        return false;
    };
    skills
        .get(first)
        .and_then(|s| s.command)
        .is_some_and(|cmd| cmd == first)
}

enum DispatchError {
    NotACommand,
    Parse(String),
    Cli(crate::error::CliError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextBuilder;
    use aura_config::AuraConfig;
    use aura_registry::{ArtifactSource, TrustLevel};
    use aura_skills::{SkillDefinition, SkillRegistry, SkillRequirements};

    fn skill(name: &str, description: &str, user_invocable: bool) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            version: "0.1.0".into(),
            description: description.into(),
            command: if user_invocable {
                Some(name.into())
            } else {
                None
            },
            agent_invocable: true,
            argument_hint: None,
            prompt_template: "body".into(),
            allowed_tools: vec![],
            source: ArtifactSource::Workspace,
            trust_level: TrustLevel::Trusted,
            requirements: SkillRequirements::default(),
            token_budget_hint: 1024,
            source_path: None,
        }
    }

    fn handler_with(registry: SkillRegistry) -> CliSlashHandler {
        let config = Arc::new(AuraConfig::default());
        let ctx = ContextBuilder::new(config)
            .skills(Arc::new(registry))
            .build();
        CliSlashHandler::new(Arc::new(ctx))
    }

    #[test]
    fn commands_list_includes_user_invocable_skills() {
        let reg = SkillRegistry::new();
        reg.register(skill("greet", "say hi", true));
        reg.register(skill("hidden", "model-only skill", false));
        let handler = handler_with(reg);

        let cmds = handler.commands();
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"/greet"), "expected /greet in: {names:?}");
        assert!(
            !names.contains(&"/hidden"),
            "skills with user-invocable: false must not surface"
        );
    }

    #[test]
    fn commands_list_uses_argument_hint_when_present() {
        let reg = SkillRegistry::new();
        let mut s = skill("fix-issue", "fix a GitHub issue", true);
        s.argument_hint = Some("[issue-number]".into());
        reg.register(s);
        let handler = handler_with(reg);

        let cmds = handler.commands();
        let entry = cmds
            .iter()
            .find(|c| c.name == "/fix-issue")
            .expect("skill listed");
        assert!(
            entry.description.contains("[issue-number]"),
            "description should surface argument hint, got: {}",
            entry.description
        );
        assert!(entry.description.contains("fix a GitHub issue"));
    }

    #[test]
    fn clap_subcommand_wins_collision_with_skill() {
        // `config` is a real clap subcommand; a workspace skill with the same
        // name must not duplicate or shadow it.
        let reg = SkillRegistry::new();
        reg.register(skill("config", "impersonate built-in", true));
        let handler = handler_with(reg);

        let cmds = handler.commands();
        let config_entries: Vec<&SlashCommand> =
            cmds.iter().filter(|c| c.name == "/config").collect();
        assert_eq!(
            config_entries.len(),
            1,
            "expected exactly one /config entry, got {}",
            config_entries.len()
        );
        assert_ne!(
            config_entries[0].description, "impersonate built-in",
            "skill description must not overwrite the clap subcommand description"
        );
    }

    #[tokio::test]
    async fn skill_slash_is_passed_through_to_agent() {
        let reg = SkillRegistry::new();
        reg.register(skill("greet", "say hi", true));
        let handler = handler_with(reg);

        match handler.handle("/greet").await {
            SlashOutcome::PassThrough => {}
            other => panic!("expected PassThrough, got {other:?}"),
        }

        // Args after the skill name should still pass through.
        match handler.handle("/greet Alice").await {
            SlashOutcome::PassThrough => {}
            other => panic!("expected PassThrough with args, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skill_slash_ignores_non_user_invocable() {
        // `disable-model-invocation: true` + `user-invocable: false` would
        // make `command` None, so the slash should hit the clap path and
        // come back as a parse error — not a PassThrough.
        let reg = SkillRegistry::new();
        reg.register(skill("hidden", "model-only", false));
        let handler = handler_with(reg);

        match handler.handle("/hidden").await {
            SlashOutcome::PassThrough => panic!("hidden skill must not be invocable via slash"),
            SlashOutcome::Handled(_) => {}
            other => panic!("expected Handled error, got {other:?}"),
        }
    }
}
