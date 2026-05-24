//! External-agent quick-setup. Probes `claude` + `codex` + `gemini` on
//! PATH, then shows the detected ones in a single multi-select so the
//! operator enables the set they want in one pass; if more than one
//! ends up enabled, prompts for the default. Records each enabled
//! agent's discovered binary path.

use aura_agent::external_agent::claude_cli::ClaudeCliAgent;
use aura_agent::external_agent::codex_cli::CodexCliAgent;
use aura_agent::external_agent::gemini_cli::GeminiCliAgent;
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
    select_and_apply(prompter, config, detect_on_path())
}

/// Core split out of [`configure_external_agents_step`] so it can be
/// unit-tested with a synthetic `detected` list — `detect_on_path`
/// itself walks the real `PATH` and can't be scripted.
fn select_and_apply<P: Prompter>(
    prompter: &mut P,
    config: &mut AuraConfig,
    detected: Vec<Detected>,
) -> Result<ExternalAgentsStepOutcome> {
    if detected.is_empty() {
        eprintln!("No external agents (`claude`, `codex`, `gemini`) found on PATH; skipping.");
        return Ok(ExternalAgentsStepOutcome::default());
    }

    // Pre-check agents already enabled in config; on a fresh install
    // (nothing enabled yet) pre-check everything detected, since the
    // operator just confirmed these are installed.
    let preselected: Vec<bool> = detected
        .iter()
        .map(|d| is_enabled(config, d.kind))
        .collect();
    let initial = if preselected.iter().any(|&on| on) {
        preselected
    } else {
        vec![true; detected.len()]
    };

    let labels: Vec<String> = detected
        .iter()
        .map(|d| format!("{} — {}", d.kind.display_name(), d.binary_path))
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let picked = prompter.multi_select(
        "Enable external agents (↑/↓ move, space toggles, enter confirms):",
        &label_refs,
        &initial,
    )?;

    let mut outcome = ExternalAgentsStepOutcome::default();
    for (i, d) in detected.iter().enumerate() {
        let on = picked.contains(&i);
        apply_enable(config, d, on);
        if on {
            outcome.enabled.push(d.kind);
        }
    }

    outcome.default = pick_default(prompter, config)?;
    Ok(outcome)
}

/// Resolve `default_external_agent` over everything enabled in config
/// (which may include an enabled kind we didn't re-detect this run):
/// `None` when nothing is enabled, the sole entry when exactly one is,
/// and an explicit pick when more than one competes.
fn pick_default<P: Prompter>(
    prompter: &mut P,
    config: &mut AuraConfig,
) -> Result<Option<ExternalAgentKind>> {
    let enabled = config.external_agents.enabled_kinds();
    let chosen = match enabled.as_slice() {
        [] => None,
        [only] => Some(*only),
        many => {
            let labels: Vec<&str> = many.iter().map(|k| k.display_name()).collect();
            let idx = prompter.select("Pick a default external agent:", &labels)?;
            Some(many[idx])
        }
    };
    config.external_agents.default_external_agent = chosen;
    Ok(chosen)
}

struct Detected {
    kind: ExternalAgentKind,
    binary_path: String,
}

fn detect_on_path() -> Vec<Detected> {
    let mut out = Vec::new();
    for kind in ExternalAgentKind::ALL.iter().copied() {
        if let Some(path) = probe(kind) {
            out.push(Detected {
                kind,
                binary_path: path,
            });
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
        ExternalAgentKind::Gemini => GeminiCliAgent::probe_and_build(None)
            .ok()
            .map(|a| a.binary_path().display().to_string()),
    }
}

fn is_enabled(config: &AuraConfig, kind: ExternalAgentKind) -> bool {
    match kind {
        ExternalAgentKind::Claude => config.external_agents.claude.enabled,
        ExternalAgentKind::Codex => config.external_agents.codex.enabled,
        ExternalAgentKind::Gemini => config.external_agents.gemini.enabled,
    }
}

fn apply_enable(config: &mut AuraConfig, d: &Detected, on: bool) {
    let (enabled, binary_path) = match d.kind {
        ExternalAgentKind::Claude => (
            &mut config.external_agents.claude.enabled,
            &mut config.external_agents.claude.binary_path,
        ),
        ExternalAgentKind::Codex => (
            &mut config.external_agents.codex.enabled,
            &mut config.external_agents.codex.binary_path,
        ),
        ExternalAgentKind::Gemini => (
            &mut config.external_agents.gemini.enabled,
            &mut config.external_agents.gemini.binary_path,
        ),
    };
    *enabled = on;
    if on {
        *binary_path = Some(d.binary_path.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockPrompter;

    fn detected(kinds: &[ExternalAgentKind]) -> Vec<Detected> {
        kinds
            .iter()
            .map(|&kind| Detected {
                kind,
                binary_path: format!("/usr/bin/{}", kind.binary_name()),
            })
            .collect()
    }

    #[test]
    fn enables_only_the_selected_subset() {
        let mut config = AuraConfig::default();
        // Two detected, check only index 1 (codex).
        let mut prompter = MockPrompter::new().push_multi_select(vec![1]);
        let outcome = select_and_apply(
            &mut prompter,
            &mut config,
            detected(&[ExternalAgentKind::Claude, ExternalAgentKind::Codex]),
        )
        .unwrap();

        assert!(!config.external_agents.claude.enabled);
        assert!(config.external_agents.codex.enabled);
        assert_eq!(
            config.external_agents.codex.binary_path.as_deref(),
            Some("/usr/bin/codex")
        );
        assert_eq!(outcome.enabled, vec![ExternalAgentKind::Codex]);
        // Single enabled → implicit default, no select prompt consumed.
        assert_eq!(outcome.default, Some(ExternalAgentKind::Codex));
    }

    #[test]
    fn multiple_enabled_prompts_for_default() {
        let mut config = AuraConfig::default();
        // Check both (0,1), then pick default = index 1 (codex).
        let mut prompter = MockPrompter::new()
            .push_multi_select(vec![0, 1])
            .push_select(1);
        let outcome = select_and_apply(
            &mut prompter,
            &mut config,
            detected(&[ExternalAgentKind::Claude, ExternalAgentKind::Codex]),
        )
        .unwrap();

        assert!(config.external_agents.claude.enabled);
        assert!(config.external_agents.codex.enabled);
        assert_eq!(outcome.default, Some(ExternalAgentKind::Codex));
        assert_eq!(
            config.external_agents.default_external_agent,
            Some(ExternalAgentKind::Codex)
        );
    }

    #[test]
    fn deselecting_all_disables_and_clears_default() {
        let mut config = AuraConfig::default();
        config.external_agents.claude.enabled = true;
        config.external_agents.default_external_agent = Some(ExternalAgentKind::Claude);

        // Detected claude is shown pre-checked; uncheck everything.
        let mut prompter = MockPrompter::new().push_multi_select(vec![]);
        let outcome = select_and_apply(
            &mut prompter,
            &mut config,
            detected(&[ExternalAgentKind::Claude]),
        )
        .unwrap();

        assert!(!config.external_agents.claude.enabled);
        assert!(outcome.enabled.is_empty());
        assert_eq!(outcome.default, None);
        assert_eq!(config.external_agents.default_external_agent, None);
    }

    #[test]
    fn empty_detection_is_a_noop() {
        let mut config = AuraConfig::default();
        let mut prompter = MockPrompter::new();
        let outcome = select_and_apply(&mut prompter, &mut config, Vec::new()).unwrap();
        assert_eq!(outcome, ExternalAgentsStepOutcome::default());
    }
}
