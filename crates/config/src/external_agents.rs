//! Per-external-agent operator config.

use aura_model::ExternalAgentKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalAgentsConfig {
    pub claude: ClaudeConfig,
    pub codex: CodexConfig,
    /// Which kind to treat as the operator's primary when multiple
    /// external agents are configured. Only meaningful (and only
    /// validated) when more than one kind has a non-default config;
    /// today nothing in the spawn protocol reads it — it's an
    /// operator-visible designation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_external_agent: Option<ExternalAgentKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    /// Whether boot should probe + register this agent. Default
    /// `false` is the trust signal: an installed `claude` on PATH
    /// does NOT auto-grant the LLM access to a host-execution
    /// channel — the operator must opt in explicitly (via
    /// `aura external-agent setup`, `aura setup`, or by editing
    /// `aura.json`).
    pub enabled: bool,
    /// Path to the `claude` binary. `None` falls back to `PATH`
    /// lookup. Only consulted when `enabled = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    /// Whether boot should probe + register this agent. See the
    /// `ClaudeConfig::enabled` docstring for rationale.
    pub enabled: bool,
    /// Path to the `codex` binary. `None` falls back to `PATH`
    /// lookup. Only consulted when `enabled = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

impl ExternalAgentsConfig {
    /// Which kinds the operator has explicitly opted into. Boot
    /// probes / registers only these. Validation also keys on this
    /// when enforcing `default_external_agent` for the multi-enabled
    /// case.
    pub fn enabled_kinds(&self) -> Vec<ExternalAgentKind> {
        let mut out = Vec::new();
        if self.claude.enabled {
            out.push(ExternalAgentKind::Claude);
        }
        if self.codex.enabled {
            out.push(ExternalAgentKind::Codex);
        }
        out
    }

    /// One entry per `ExternalAgentKind`, in `ALL` order. The boot
    /// path feeds this to `aura_agent::external_agent::build_registry`
    /// without further per-kind translation. Adding a new kind: extend
    /// `ExternalAgentKind::ALL`, add a config struct field, extend
    /// this iterator.
    pub fn boot_entries(&self) -> Vec<(ExternalAgentKind, bool, Option<&str>)> {
        vec![
            (
                ExternalAgentKind::Claude,
                self.claude.enabled,
                self.claude.binary_path.as_deref(),
            ),
            (
                ExternalAgentKind::Codex,
                self.codex.enabled,
                self.codex.binary_path.as_deref(),
            ),
        ]
    }
}
