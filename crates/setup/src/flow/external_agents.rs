//! External-agent setup. Probes `claude` + `codex` on PATH, then shows
//! the detected ones in a single multi-select so the operator confirms
//! (or withholds) the set in one pass. Records each enabled agent's
//! discovered **absolute** binary path, so the gateway service — which
//! may run with a different cwd and a narrower `PATH` — pins the same
//! binary the operator just probed instead of re-walking `PATH`.

use baybo_agent::external_agent::claude_cli::ClaudeCliAgent;
use baybo_agent::external_agent::codex_cli::CodexCliAgent;
use baybo_config::BayboConfig;
use baybo_model::ExternalAgentKind;

use crate::error::Result;
use crate::prompt::Prompter;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExternalAgentsStepOutcome {
    pub enabled: Vec<ExternalAgentKind>,
}

pub async fn configure_external_agents_step<P: Prompter>(
    prompter: &mut P,
    config: &mut BayboConfig,
) -> Result<ExternalAgentsStepOutcome> {
    select_and_apply(prompter, config, detect_on_path().await)
}

/// Core split out of [`configure_external_agents_step`] so it can be
/// unit-tested with a synthetic `detected` list — `detect_on_path`
/// itself walks the real `PATH` and can't be scripted.
fn select_and_apply<P: Prompter>(
    prompter: &mut P,
    config: &mut BayboConfig,
    detected: Vec<Detected>,
) -> Result<ExternalAgentsStepOutcome> {
    if detected.is_empty() {
        eprintln!("No external agents (`claude`, `codex`) found on PATH; skipping.");
        return Ok(ExternalAgentsStepOutcome::default());
    }

    // Pre-check each detected agent to its current `enabled` state.
    // Since every kind ships enabled, a fresh install pre-checks
    // everything detected on its own — and an operator who previously
    // unchecked one keeps it unchecked, rather than having setup
    // silently switch it back on.
    let initial: Vec<bool> = detected
        .iter()
        .map(|d| is_enabled(config, d.kind))
        .collect();

    let labels: Vec<String> = detected
        .iter()
        .map(|d| format!("{} — {}", d.kind.display_name(), d.binary_path))
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let picked = prompter.multi_select("Enable external agents:", &label_refs, &initial)?;

    let mut outcome = ExternalAgentsStepOutcome::default();
    for (i, d) in detected.iter().enumerate() {
        let on = picked.contains(&i);
        apply_enable(config, d, on);
        if on {
            outcome.enabled.push(d.kind);
        }
    }

    Ok(outcome)
}

struct Detected {
    kind: ExternalAgentKind,
    binary_path: String,
}

async fn detect_on_path() -> Vec<Detected> {
    let mut out = Vec::new();
    for kind in ExternalAgentKind::ALL.iter().copied() {
        if let Some(path) = probe(kind).await {
            out.push(Detected {
                kind,
                binary_path: path,
            });
        }
    }
    out
}

async fn probe(kind: ExternalAgentKind) -> Option<String> {
    match kind {
        ExternalAgentKind::Claude => {
            ClaudeCliAgent::probe_and_build(baybo_process::ProcessManager::transient(), None, None)
                .await
                .ok()
                .map(|a| a.binary_path().display().to_string())
        }
        ExternalAgentKind::Codex => {
            CodexCliAgent::probe_and_build(baybo_process::ProcessManager::transient(), None, None)
                .await
                .ok()
                .map(|a| a.binary_path().display().to_string())
        }
    }
}

fn is_enabled(config: &BayboConfig, kind: ExternalAgentKind) -> bool {
    match kind {
        ExternalAgentKind::Claude => config.external_agents.claude.enabled,
        ExternalAgentKind::Codex => config.external_agents.codex.enabled,
    }
}

fn apply_enable(config: &mut BayboConfig, d: &Detected, on: bool) {
    let (enabled, binary_path) = match d.kind {
        ExternalAgentKind::Claude => (
            &mut config.external_agents.claude.enabled,
            &mut config.external_agents.claude.binary_path,
        ),
        ExternalAgentKind::Codex => (
            &mut config.external_agents.codex.enabled,
            &mut config.external_agents.codex.binary_path,
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
        let mut config = BayboConfig::default();
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
    }

    /// The multi-select is the whole step — checking every box must not
    /// draw a second prompt. `MockPrompter` panics on an unscripted
    /// call, so a stray follow-up question fails this test.
    #[test]
    fn checking_every_box_asks_nothing_further() {
        let mut config = BayboConfig::default();
        let mut prompter = MockPrompter::new().push_multi_select(vec![0, 1]);
        let outcome = select_and_apply(
            &mut prompter,
            &mut config,
            detected(&[ExternalAgentKind::Claude, ExternalAgentKind::Codex]),
        )
        .unwrap();

        assert!(config.external_agents.claude.enabled);
        assert!(config.external_agents.codex.enabled);
        assert_eq!(
            outcome.enabled,
            vec![ExternalAgentKind::Claude, ExternalAgentKind::Codex],
        );
    }

    #[test]
    fn deselecting_all_disables_every_detected_kind() {
        let mut config = BayboConfig::default();
        config.external_agents.claude.enabled = true;

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
    }

    #[test]
    fn empty_detection_is_a_noop() {
        let mut config = BayboConfig::default();
        let mut prompter = MockPrompter::new();
        let outcome = select_and_apply(&mut prompter, &mut config, Vec::new()).unwrap();
        assert_eq!(outcome, ExternalAgentsStepOutcome::default());
    }
}
