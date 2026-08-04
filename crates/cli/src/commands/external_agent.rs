use std::io::IsTerminal;
use std::path::PathBuf;

use baybo_agent::external_agent::{
    ExternalAgentError, claude_cli::ClaudeCliAgent, codex_cli::CodexCliAgent,
};
use baybo_config::BayboConfig;
use baybo_model::ExternalAgentKind;
use baybo_workspace::paths::{ENV_CONFIG_PATH, default_config_file};
use serde_json::{Value, json};

use crate::cli::ExternalAgentCmd;
use crate::commands::prompt::prompt_with_default;
use crate::commands::select::{select_many, select_one};
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

const RESTART_HINT: &str =
    "\n(note: a running `baybo gateway` keeps the old config; restart it to pick up)";

pub async fn handle(ctx: &CommandContext, cmd: ExternalAgentCmd) -> Result<CommandOutput> {
    match cmd {
        ExternalAgentCmd::Status => status(ctx),
        ExternalAgentCmd::Setup => setup(ctx).await,
        ExternalAgentCmd::Disable => disable(ctx).await,
    }
}

fn status(ctx: &CommandContext) -> Result<CommandOutput> {
    let mut human = String::new();
    let mut data: Vec<Value> = Vec::new();
    for kind in ExternalAgentKind::ALL.iter().copied() {
        let row = describe_kind(ctx, kind);
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
            "{kind} :: {enabled_label}  binary_path={path_label}  probe={probe_label}\n",
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
        }));
    }
    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({
            "external_agents": data,
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
        &format!("`{}` binary path (empty = PATH lookup)", kind.binary_name()),
        "",
    )?;
    let binary_override = if binary_input.trim().is_empty() {
        None
    } else {
        Some(binary_input.trim().to_string())
    };

    // Persist the path the probe *resolved* to, not the operator's raw
    // answer. An empty answer means "find it on PATH"; storing that as
    // an absent `binary_path` would make the gateway service re-walk
    // PATH at boot — under a different cwd and possibly a narrower PATH
    // than this CLI. Pinning the concrete absolute location setup just
    // validated keeps both sides on the same binary.
    let binary_path = match probe(kind, binary_override.as_deref()) {
        Ok(resolved) => ensure_absolute(&resolved)?,
        Err(e) => {
            return Err(CliError::Manager(format!(
                "probe failed for {}: {e}",
                kind.as_str()
            )));
        }
    };

    let mut new_config: BayboConfig = ctx.config.as_ref().clone();
    match kind {
        ExternalAgentKind::Claude => {
            new_config.external_agents.claude.enabled = true;
            new_config.external_agents.claude.binary_path = Some(binary_path.clone());
        }
        ExternalAgentKind::Codex => {
            new_config.external_agents.codex.enabled = true;
            new_config.external_agents.codex.binary_path = Some(binary_path.clone());
        }
    }

    new_config
        .validate()
        .map_err(|e| CliError::Config(format!("config validation failed: {e}")))?;
    new_config.write_to_file(&target).await?;

    let mut human = format!(
        "set up external agent {} :: binary_path={binary_path}\n",
        kind.as_str(),
    );
    human.push_str(RESTART_HINT);

    Ok(CommandOutput {
        human,
        data: Some(json!({
            "kind": kind.as_str(),
            "binary_path": binary_path,
            "requires_restart": true,
        })),
    })
}

async fn disable(ctx: &CommandContext) -> Result<CommandOutput> {
    require_tty()?;
    let target = resolve_target_path(ctx)?;

    let enabled = ctx.config.external_agents.enabled_kinds();
    if enabled.is_empty() {
        // Nothing enabled means the feature is already off — that's the
        // goal of `disable`, so report success, not an error.
        return Ok(CommandOutput {
            human: "no external agents are enabled — the feature is already off".to_string(),
            data: Some(json!({ "disabled": [], "requires_restart": false })),
        });
    }

    let labels: Vec<&str> = enabled.iter().map(|k| k.display_name()).collect();
    let initial = vec![false; enabled.len()];
    let picks = select_many("Select external agents to disable:", &labels, &initial)?;
    if picks.is_empty() {
        return Ok(CommandOutput {
            human: "nothing selected; no external agents were disabled".to_string(),
            data: Some(json!({ "disabled": [], "requires_restart": false })),
        });
    }
    let kinds: Vec<ExternalAgentKind> = picks.iter().map(|&i| enabled[i]).collect();

    let mut new_config: BayboConfig = ctx.config.as_ref().clone();
    for kind in &kinds {
        match kind {
            ExternalAgentKind::Claude => new_config.external_agents.claude.enabled = false,
            ExternalAgentKind::Codex => new_config.external_agents.codex.enabled = false,
        }
    }

    new_config
        .validate()
        .map_err(|e| CliError::Config(format!("config validation failed: {e}")))?;
    new_config.write_to_file(&target).await?;

    let disabled: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
    let mut human = format!("disabled external agent(s): {}\n", disabled.join(", "));
    human.push_str(RESTART_HINT);

    Ok(CommandOutput {
        human,
        data: Some(json!({
            "disabled": disabled,
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
            let agent = ClaudeCliAgent::probe_and_build(binary_path, None)?;
            Ok(agent.binary_path().display().to_string())
        }
        ExternalAgentKind::Codex => {
            let agent = CodexCliAgent::probe_and_build(binary_path, None)?;
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

/// Pin a probe-resolved binary location to an absolute path before it's
/// written to config. `resolve_binary` rejects a relative `binary_path`
/// at boot (it would resolve against whatever cwd the gateway service
/// has, not this CLI's), so a PATH walk that yielded a relative entry is
/// made absolute here. Already-absolute paths pass through verbatim.
fn ensure_absolute(resolved: &str) -> Result<String> {
    let path = std::path::Path::new(resolved);
    if path.is_absolute() {
        return Ok(resolved.to_string());
    }
    std::path::absolute(path)
        .map(|p| p.display().to_string())
        .map_err(|e| {
            CliError::Config(format!(
                "could not resolve {resolved:?} to an absolute path: {e}"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_absolute_passes_absolute_path_through_verbatim() {
        let got = ensure_absolute("/usr/local/bin/claude").expect("absolute path is accepted");
        assert_eq!(got, "/usr/local/bin/claude");
    }

    #[test]
    fn ensure_absolute_promotes_relative_path() {
        let got = ensure_absolute("bin/claude").expect("relative path resolves against cwd");
        assert!(
            std::path::Path::new(&got).is_absolute(),
            "expected an absolute path, got {got:?}"
        );
        assert!(got.ends_with("bin/claude"), "got {got:?}");
    }
}
