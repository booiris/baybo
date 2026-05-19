//! External-agent quick-setup. Probes `claude` + `codex` on PATH and
//! lets the operator toggle each on, recording the discovered binary
//! path. If multiple end up enabled, prompts for the default.

use aura_agent::external_agent::claude_cli::ClaudeCliAgent;
use aura_agent::external_agent::codex_cli::CodexCliAgent;
use aura_config::AuraConfig;
use aura_model::ExternalAgentKind;

use crate::error::Result;
use crate::prompt::Prompter;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExternalAgentsStepOutcome {
    pub enabled: Vec<ExternalAgentKind>,
    pub default: Option<ExternalAgentKind>,
}

pub async fn configure_external_agents_step<P: Prompter>(
    prompter: &mut P,
    config: &mut AuraConfig,
) -> Result<ExternalAgentsStepOutcome> {
    let detected = detect_on_path();
    if detected.is_empty() {
        eprintln!("No external agents (`claude`, `codex`) found on PATH; skipping.");
        return Ok(ExternalAgentsStepOutcome::default());
    }

    let mut outcome = ExternalAgentsStepOutcome::default();
    for d in &detected {
        let label = format!(
            "Detected {} at {}. Enable as external agent?",
            d.kind.display_name(),
            d.binary_path,
        );
        if prompter.confirm(&label, true)? {
            apply_enable(config, d);
            outcome.enabled.push(d.kind);
        }
    }

    if outcome.enabled.len() > 1 {
        let labels: Vec<&str> = outcome
            .enabled
            .iter()
            .map(|k| k.display_name())
            .collect();
        let idx = prompter.select("Pick a default external agent:", &labels)?;
        let chosen = outcome.enabled[idx];
        config.external_agents.default_external_agent = Some(chosen);
        outcome.default = Some(chosen);
    } else if let Some(only) = outcome.enabled.first() {
        // Single-enabled is implicitly default; record it so
        // `aura external-agent status` shows the marker.
        config.external_agents.default_external_agent = Some(*only);
        outcome.default = Some(*only);
    }

    Ok(outcome)
}

struct Detected {
    kind: ExternalAgentKind,
    binary_path: String,
}

fn detect_on_path() -> Vec<Detected> {
    let mut out = Vec::new();
    for kind in ExternalAgentKind::ALL.iter().copied() {
        match probe(kind) {
            Some(path) => out.push(Detected {
                kind,
                binary_path: path,
            }),
            None => continue,
        }
    }
    out
}

fn probe(kind: ExternalAgentKind) -> Option<String> {
    match kind {
        ExternalAgentKind::Claude => ClaudeCliAgent::probe_and_build(None)
            .ok()
            .map(|a| a.binary_path().display().to_string()),
        ExternalAgentKind::Codex => CodexCliAgent::probe_and_build(None)
            .ok()
            .map(|a| a.binary_path().display().to_string()),
    }
}

fn apply_enable(config: &mut AuraConfig, d: &Detected) {
    match d.kind {
        ExternalAgentKind::Claude => {
            config.external_agents.claude.enabled = true;
            config.external_agents.claude.binary_path = Some(d.binary_path.clone());
        }
        ExternalAgentKind::Codex => {
            config.external_agents.codex.enabled = true;
            config.external_agents.codex.binary_path = Some(d.binary_path.clone());
        }
    }
}
