use std::io::IsTerminal;
use std::path::PathBuf;

use aura_agent::external_agent::{
    ExternalAgentError, claude_cli::ClaudeCliAgent, codex_cli::CodexCliAgent,
};
use aura_config::AuraConfig;
use aura_model::ExternalAgentKind;
use aura_workspace::paths::{ENV_CONFIG_PATH, default_config_file};
use serde_json::{Value, json};

use crate::cli::ExternalAgentCmd;
use crate::commands::prompt::prompt_with_default;
use crate::commands::select::select_one;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

const RESTART_HINT: &str =
    "\n(note: a running `aura gateway` keeps the old config; restart it to pick up)";

pub async fn handle(ctx: &CommandContext, cmd: ExternalAgentCmd) -> Result<CommandOutput> {
    match cmd {
        ExternalAgentCmd::Status => status(ctx),
        ExternalAgentCmd::Setup => setup(ctx).await,
    }
}

fn status(ctx: &CommandContext) -> Result<CommandOutput> {
    let mut human = String::new();
    let mut data: Vec<Value> = Vec::new();
    let default = ctx.config.external_agents.default_external_agent;
    for kind in ExternalAgentKind::ALL.iter().copied() {
        let row = describe_kind(ctx, kind);
        let marker = if default == Some(kind) { "*" } else { " " };
        let enabled_label = if row.enabled { "enabled" } else { "DISABLED" };
        let probe_label = match &row.probe {
            ProbeOutcome::Ok { resolved_path } => format!("ok ({resolved_path})"),
            ProbeOutcome::NotInstalled => "not installed".into(),
            ProbeOutcome::Failed(msg) => format!("FAIL: {msg}"),
        };
        let path_label = row
            .configured_path
            .clone()
            .unwrap_or_else(|| "(PATH lookup)".to_string());
        human.push_str(&format!(
            "{marker} {kind} :: {enabled_label}  binary_path={path_label}  probe={probe_label}\n",
            kind = kind.as_str(),
        ));
        data.push(json!({
            "kind": kind.as_str(),
            "enabled": row.enabled,
            "configured_binary_path": row.configured_path,
            "probe": match &row.probe {
                ProbeOutcome::Ok { resolved_path } => {
                    json!({"status": "ok", "resolved_path": resolved_path})
                }
                ProbeOutcome::NotInstalled => json!({"status": "not_installed"}),
                ProbeOutcome::Failed(msg) => json!({"status": "failed", "error": msg}),
            },
            "is_default": default == Some(kind),
        }));
    }
    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({
            "external_agents": data,
            "default_external_agent": default.map(|k| k.as_str()),
        })),
    })
}

async fn setup(ctx: &CommandContext) -> Result<CommandOutput> {
    require_tty()?;
    let target = resolve_target_path(ctx)?;
    let labels: Vec<&str> = ExternalAgentKind::ALL
        .iter()
        .map(|k| k.display_name())
        .collect();
    let idx = select_one("Pick an external agent to set up:", &labels)?;
    let kind = ExternalAgentKind::ALL[idx];

    let binary_input = prompt_with_default(
        &format!(
            "`{}` binary path (empty = PATH lookup)",
            kind.binary_name()
        ),
        "",
    )?;
    let binary_path = if binary_input.trim().is_empty() {
        None
    } else {
        Some(binary_input.trim().to_string())
    };

    match probe(kind, binary_path.as_deref()) {
        Ok(_) => {}
        Err(e) => {
            return Err(CliError::Manager(format!(
                "probe failed for {}: {e}",
                kind.as_str()
            )));
        }
    }

    let mut new_config: AuraConfig = ctx.config.as_ref().clone();
    match kind {
        ExternalAgentKind::Claude => {
            new_config.external_agents.claude.enabled = true;
            new_config.external_agents.claude.binary_path = binary_path.clone();
        }
        ExternalAgentKind::Codex => {
            new_config.external_agents.codex.enabled = true;
            new_config.external_agents.codex.binary_path = binary_path.clone();
        }
    }

    // If multiple kinds are enabled after this write and no default
    // is set, require the operator to pick one.
    let enabled = new_config.external_agents.enabled_kinds();
    if enabled.len() > 1
        && new_config
            .external_agents
            .default_external_agent
            .filter(|d| enabled.contains(d))
            .is_none()
    {
        let default_labels: Vec<&str> = enabled.iter().map(|k| k.display_name()).collect();
        let default_idx = select_one(
            "Multiple external agents are now enabled; pick the default:",
            &default_labels,
        )?;
        new_config.external_agents.default_external_agent = Some(enabled[default_idx]);
    }

    new_config
        .validate()
        .map_err(|e| CliError::Config(format!("config validation failed: {e}")))?;
    new_config.write_to_file(&target).await?;

    let mut human = format!(
        "set up external agent {} :: binary_path={}\n",
        kind.as_str(),
        binary_path.as_deref().unwrap_or("(PATH lookup)"),
    );
    if let Some(d) = new_config.external_agents.default_external_agent {
        human.push_str(&format!("default_external_agent={}\n", d.as_str()));
    }
    human.push_str(RESTART_HINT);

    Ok(CommandOutput {
        human,
        data: Some(json!({
            "kind": kind.as_str(),
            "binary_path": binary_path,
            "default_external_agent": new_config
                .external_agents
                .default_external_agent
                .map(|k| k.as_str()),
            "requires_restart": true,
        })),
    })
}

struct StatusRow {
    enabled: bool,
    configured_path: Option<String>,
    probe: ProbeOutcome,
}

enum ProbeOutcome {
    Ok { resolved_path: String },
    NotInstalled,
    Failed(String),
}

fn describe_kind(ctx: &CommandContext, kind: ExternalAgentKind) -> StatusRow {
    let (enabled, configured_path) = match kind {
        ExternalAgentKind::Claude => (
            ctx.config.external_agents.claude.enabled,
            ctx.config.external_agents.claude.binary_path.clone(),
        ),
        ExternalAgentKind::Codex => (
            ctx.config.external_agents.codex.enabled,
            ctx.config.external_agents.codex.binary_path.clone(),
        ),
    };
    let probe = match probe(kind, configured_path.as_deref()) {
        Ok(resolved_path) => ProbeOutcome::Ok { resolved_path },
        Err(ExternalAgentError::NotInstalled(_)) => ProbeOutcome::NotInstalled,
        Err(e) => ProbeOutcome::Failed(e.to_string()),
    };
    StatusRow {
        enabled,
        configured_path,
        probe,
    }
}

/// Probe + discard. Returns the resolved binary's display path on
/// success — that's what the status output renders. The agent itself
/// is built and thrown away; we just want the side-effect of running
/// the same checks boot does.
fn probe(
    kind: ExternalAgentKind,
    binary_path: Option<&str>,
) -> std::result::Result<String, ExternalAgentError> {
    match kind {
        ExternalAgentKind::Claude => {
            let agent = ClaudeCliAgent::probe_and_build(binary_path)?;
            Ok(agent.binary_path().display().to_string())
        }
        ExternalAgentKind::Codex => {
            let agent = CodexCliAgent::probe_and_build(binary_path)?;
            Ok(agent.binary_path().display().to_string())
        }
    }
}

fn resolve_target_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config_path
        .clone()
        .or_else(|| std::env::var(ENV_CONFIG_PATH).ok().map(PathBuf::from))
        .or_else(|| Some(default_config_file()))
        .ok_or_else(|| {
            CliError::Config(format!(
                "no config file resolved; set {ENV_CONFIG_PATH} (or pass --config <path>)"
            ))
        })
}

fn require_tty() -> Result<()> {
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive external-agent command requires a terminal".into(),
        ));
    }
    Ok(())
}
