use std::sync::Arc;

use async_trait::async_trait;
use baybo_channels::{SlashCommand, SlashHandler, SlashOutcome, ViewKind};
use baybo_model::ContentBlock;
use clap::{CommandFactory, Parser};

use crate::cli::{
    ChannelCmd, Cli, Commands, ConfigCmd, CostCmd, CronCmd, DeviceCmd, ExternalAgentCmd, JobCmd,
    LlmCmd, LogCmd, McpCmd, PairCmd, SecretCmd, SessionCmd, SkillsCmd,
};
use crate::context::{CommandContext, Invocation};
use crate::dispatch;
use crate::error::CliError;
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
        argv.push("baybo".to_string());
        argv.extend(tokens);
        let cli = Cli::try_parse_from(&argv).map_err(|e| DispatchError::Parse(e.to_string()))?;
        let cmd = cli.command.ok_or(DispatchError::NotACommand)?;

        // Whitelist gate. Anything not on the list is rejected before
        // a handler can run — closes the `--follow` DoS hole (a slash
        // task would otherwise tail-loop forever) and gives a clear
        // "not available in slash" error for process-lifecycle and
        // TTY-only commands instead of either hanging or producing a
        // confusing handler-side error.
        if let Err(reason) = slash_admissible(&cmd) {
            return Err(DispatchError::Cli(CliError::NotAvailableInSlash(
                reason.into(),
            )));
        }

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
            trace: self.ctx.trace.clone(),
            query_api: self.ctx.query_api.clone(),
            security: self.ctx.security.clone(),
            leak_detector: self.ctx.leak_detector.clone(),
            skill_assessor: self.ctx.skill_assessor.clone(),
            channel_bot_store: self.ctx.channel_bot_store.clone(),
            channel_pairing_store: self.ctx.channel_pairing_store.clone(),
            device_pairing_service: self.ctx.device_pairing_service.clone(),
            secret_vault: self.ctx.secret_vault.clone(),
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
            let name = sub.get_name();
            // Top-level families never admissible from slash. Listing
            // them in the menu would just bait users into typing a
            // command the dispatcher will reject. The dispatch-time
            // whitelist (`slash_admissible`) is the canonical guard;
            // this is the cosmetic mirror so the menu stays coherent.
            if matches!(
                name,
                "help" | "completion" | "setup" | "tui" | "gateway" | "memory"
            ) {
                continue;
            }
            let slash = format!("/{name}");
            // Hidden subcommands (the `BAYBO_HELP_AGENT` extended-help
            // bucket: `config`, `log`, `session`, `job`, `cron`, `cost`)
            // still own their slot — claim it in `seen` so a workspace
            // skill with the same name can't slip into the menu — but
            // skip the display row so the chat-completion list stays
            // focused.
            if sub.is_hide_set() {
                seen.insert(slash);
                continue;
            }
            let about = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
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
        // Bare `/skills`, `/jobs`, `/sessions` (no args)
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
        "jobs" => Some(ViewKind::Jobs),
        "sessions" => Some(ViewKind::Sessions),
        _ => None,
    }
}

/// Return true when the first whitespace-separated token after the leading
/// `/` names a user-invocable skill. Skill lookup goes through `get`, which
/// indexes by skill name; we also require the stored `command` to match so
/// skills with `user-invocable: false` are ignored.
fn is_skill_invocation(raw: &str, skills: &baybo_skills::SkillRegistry) -> bool {
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

/// Slash-mode whitelist. Default-deny: a command may run from a `/`
/// dispatch only if it is explicitly accepted here.
///
/// Rationale per category:
///
/// * **Process-lifecycle** (`tui`, `gateway`, `setup`, `completion`):
///   start subprocesses, daemons, or shell scripts. Slash adapters run
///   inside an already-live process; there is no second process for
///   these to attach to and no TTY to drive their UI.
/// * **TTY / OAuth / credential editors** (`channel add/remove`,
///   `mcp add`, `llm add/edit/remove/default`): interactive pickers
///   and OAuth flows that require a real terminal.
/// * **Unbounded output** (`log main --follow`, `log channel --follow`):
///   the tail-poll loop only exits on Ctrl-C; a slash invocation would
///   hold the dispatcher task open forever (DoS). See the adversarial
///   review on this surface.
///
/// Read-only inspection (config show/get, skills, session, job, cron,
/// cost, log non-follow, status, doctor, etc.) plus the mutating
/// commands that already require `--yes` (`config set/unset`, `pair
/// approve/revoke`, `job cancel`, …) are allowed.
fn slash_admissible(cmd: &Commands) -> Result<(), &'static str> {
    match cmd {
        // Process-lifecycle: never slash.
        Commands::Prompt { .. } => Err("`prompt` runs a one-shot turn; run it from a shell"),
        Commands::Tui { .. } => Err("`tui` opens an interactive process; run it from a shell"),
        Commands::Gateway { .. } => {
            Err("`gateway` controls the server process; run it from a shell")
        }
        Commands::Setup => Err("`setup` is the first-run wizard; run it from a shell"),
        Commands::Completion { .. } => {
            Err("`completion` emits a shell script; run it from a shell")
        }

        // TTY-only editors and OAuth flows.
        Commands::Channel {
            cmd: ChannelCmd::Add | ChannelCmd::Remove,
        } => Err("interactive channel editor; run it from a shell"),
        Commands::Mcp {
            cmd: McpCmd::Add { .. },
        } => Err("`mcp add` runs an OAuth/credential flow; run it from a shell"),
        Commands::Llm {
            cmd: LlmCmd::Add | LlmCmd::Edit | LlmCmd::Remove | LlmCmd::Default,
        } => Err("interactive LLM editor; run it from a shell"),
        // `memory` edits a non-hot-reload backend (provider, API keys, recall
        // knobs); the whole family is shell-only — never slash.
        Commands::Memory { .. } => {
            Err("`memory` commands edit a non-hot-reload backend; run them from a shell")
        }
        Commands::ExternalAgent {
            cmd: ExternalAgentCmd::Setup | ExternalAgentCmd::Disable | ExternalAgentCmd::Default,
        } => {
            Err("`external-agent setup`/`disable`/`default` are interactive; run them from a shell")
        }

        // Bounded reads + opt-in mutations: allowed.
        Commands::Config { cmd } => match cmd {
            // Every variant is bounded; `set`/`unset` already require `--yes`.
            ConfigCmd::Show { .. }
            | ConfigCmd::Validate { .. }
            | ConfigCmd::File
            | ConfigCmd::Schema
            | ConfigCmd::Get { .. }
            | ConfigCmd::Set { .. }
            | ConfigCmd::Unset { .. } => Ok(()),
        },
        Commands::Skills { cmd } => match cmd {
            SkillsCmd::List
            | SkillsCmd::Info { .. }
            | SkillsCmd::Search { .. }
            | SkillsCmd::Check { .. } => Ok(()),
        },
        Commands::Channel {
            cmd: ChannelCmd::List,
        } => Ok(()),
        Commands::Mcp {
            cmd: McpCmd::List { .. } | McpCmd::Get { .. } | McpCmd::Remove { .. },
        } => Ok(()),
        Commands::Secret { cmd } => match cmd {
            // `add` reads a masked value from the TTY; list shows masked
            // previews and delete self-guards with --yes (+ a NAME in slash).
            SecretCmd::Add { .. } => {
                Err("`secret add` reads a masked value from the terminal; run it from a shell")
            }
            SecretCmd::List | SecretCmd::Delete { .. } => Ok(()),
        },
        Commands::Pair { cmd } => match cmd {
            PairCmd::List { .. } | PairCmd::Approve { .. } | PairCmd::Revoke { .. } => Ok(()),
        },
        Commands::Device { cmd } => match cmd {
            DeviceCmd::Pair { .. }
            | DeviceCmd::Approve { .. }
            | DeviceCmd::List { .. }
            | DeviceCmd::Revoke { .. } => Ok(()),
        },
        Commands::Llm {
            cmd: LlmCmd::Status | LlmCmd::Probe { .. } | LlmCmd::LiveModel { .. },
        } => Ok(()),
        Commands::ExternalAgent {
            cmd: ExternalAgentCmd::Status,
        } => Ok(()),
        Commands::Session { cmd } => match cmd {
            SessionCmd::List
            | SessionCmd::Show { .. }
            | SessionCmd::History { .. }
            | SessionCmd::Export { .. } => Ok(()),
        },
        Commands::Job { cmd } => match cmd {
            JobCmd::List { .. } | JobCmd::Show { .. } | JobCmd::Cancel { .. } => Ok(()),
        },
        Commands::Cron { cmd } => match cmd {
            CronCmd::List | CronCmd::Show { .. } => Ok(()),
        },
        Commands::Log { cmd } => match cmd {
            LogCmd::Main { follow: true, .. } | LogCmd::Channel { follow: true, .. } => {
                Err("`--follow` streams indefinitely and is not supported in slash mode")
            }
            LogCmd::Main { .. } | LogCmd::Channel { .. } => Ok(()),
        },
        Commands::Cost {
            cmd: CostCmd::Show { .. },
        } => Ok(()),
        Commands::Status { .. } => Ok(()),
        Commands::Doctor => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextBuilder;
    use baybo_config::BayboConfig;
    use baybo_model::{ArtifactSource, TrustLevel};
    use baybo_skills::{SkillDefinition, SkillRegistry, SkillRequirements};

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
            linked_files: Default::default(),
        }
    }

    fn handler_with(registry: SkillRegistry) -> CliSlashHandler {
        let config = Arc::new(BayboConfig::default());
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
    fn visible_clap_subcommand_wins_collision_with_skill() {
        // `skills` is a visible clap subcommand; a workspace skill with the
        // same name must not duplicate or shadow it in the menu.
        let reg = SkillRegistry::new();
        reg.register(skill("skills", "impersonate built-in", true));
        let handler = handler_with(reg);

        let cmds = handler.commands();
        let entries: Vec<&SlashCommand> = cmds.iter().filter(|c| c.name == "/skills").collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one /skills entry, got {}",
            entries.len()
        );
        assert_ne!(
            entries[0].description, "impersonate built-in",
            "skill description must not overwrite the clap subcommand description"
        );
    }

    #[test]
    fn hidden_clap_subcommand_blocks_skill_from_menu() {
        // `config` is hidden from the default help (only visible under
        // `BAYBO_HELP_AGENT`); the menu drops it AND refuses to let a
        // same-named workspace skill claim the slot.
        let reg = SkillRegistry::new();
        reg.register(skill("config", "impersonate built-in", true));
        let handler = handler_with(reg);

        let cmds = handler.commands();
        let entries: Vec<&SlashCommand> = cmds.iter().filter(|c| c.name == "/config").collect();
        assert!(
            entries.is_empty(),
            "hidden built-in slot must not surface in the menu via a shadowing skill, got {entries:?}"
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
    async fn slash_rejects_log_follow() {
        // The exact DoS case from the adversarial review: `--follow`
        // would otherwise tail-loop forever inside a slash dispatcher.
        let handler = handler_with(SkillRegistry::new());
        match handler.handle("/log main --follow").await {
            SlashOutcome::Handled(blocks) => {
                let text = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                assert!(
                    text.contains("not available in slash") && text.contains("follow"),
                    "expected slash-rejection mentioning --follow, got: {text}"
                );
            }
            other => panic!("expected Handled rejection, got {other:?}"),
        }

        match handler.handle("/log channel telegram --follow").await {
            SlashOutcome::Handled(_) => {}
            other => panic!("expected Handled rejection for channel --follow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slash_rejects_process_lifecycle_commands() {
        let handler = handler_with(SkillRegistry::new());
        for raw in ["/tui", "/gateway status", "/setup"] {
            match handler.handle(raw).await {
                SlashOutcome::Handled(blocks) => {
                    let text = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    assert!(
                        text.contains("not available in slash"),
                        "expected slash-rejection for {raw}, got: {text}"
                    );
                }
                other => panic!("expected Handled rejection for {raw}, got {other:?}"),
            }
        }
    }

    #[test]
    fn slash_menu_drops_process_lifecycle_families() {
        let handler = handler_with(SkillRegistry::new());
        let names: Vec<String> = handler.commands().into_iter().map(|c| c.name).collect();
        for forbidden in ["/tui", "/gateway", "/setup", "/completion"] {
            assert!(
                !names.contains(&forbidden.to_string()),
                "{forbidden} must not appear in the slash menu, got {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn slash_excludes_memory_entirely() {
        let handler = handler_with(SkillRegistry::new());
        // Not advertised in the menu.
        let names: Vec<String> = handler.commands().into_iter().map(|c| c.name).collect();
        assert!(
            !names.contains(&"/memory".to_string()),
            "/memory must not appear in the slash menu, got {names:?}"
        );
        // And rejected at dispatch — every subcommand is shell-only.
        for raw in [
            "/memory status",
            "/memory setup",
            "/memory disable",
            "/memory test",
        ] {
            match handler.handle(raw).await {
                SlashOutcome::Handled(blocks) => {
                    let text = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    assert!(
                        text.contains("not available in slash"),
                        "expected slash-rejection for {raw}, got: {text}"
                    );
                }
                other => panic!("expected Handled rejection for {raw}, got {other:?}"),
            }
        }
    }

    #[test]
    fn slash_admissible_accepts_read_only_inspection() {
        // Spot-check the non-error arms so a future variant rename or
        // accidental whitelist tighten-up doesn't lock operators out.
        use crate::cli::{JobCmd, LogCmd, SessionCmd, SkillsCmd};
        assert!(slash_admissible(&Commands::Doctor).is_ok());
        assert!(
            slash_admissible(&Commands::Status { live: false }).is_ok()
                && slash_admissible(&Commands::Status { live: true }).is_ok()
        );
        assert!(
            slash_admissible(&Commands::Log {
                cmd: LogCmd::Main {
                    date: None,
                    limit: 50,
                    follow: false
                }
            })
            .is_ok()
        );
        assert!(
            slash_admissible(&Commands::Session {
                cmd: SessionCmd::Export {
                    id: "sess-1".into(),
                    out: None,
                    yes: false,
                },
            })
            .is_ok()
        );
        assert!(
            slash_admissible(&Commands::Skills {
                cmd: SkillsCmd::List
            })
            .is_ok()
        );
        assert!(
            slash_admissible(&Commands::Job {
                cmd: JobCmd::List { status: None }
            })
            .is_ok()
        );
        assert!(
            slash_admissible(&Commands::Session {
                cmd: SessionCmd::List
            })
            .is_ok()
        );
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
