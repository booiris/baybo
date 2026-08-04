//! Per-external-agent operator config.

use baybo_model::ExternalAgentKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalAgentsConfig {
    pub claude: ClaudeConfig,
    pub codex: CodexConfig,
}

/// Default for both per-kind `enabled` flags. Boot probes each kind on
/// `PATH` and registers only the ones actually installed, so leaving
/// this on costs nothing on a host without the binary — it just means
/// a machine that *does* have `claude` / `codex` can delegate to them
/// out of the box.
const ENABLED_BY_DEFAULT: bool = true;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    /// Whether boot should probe + register this agent. Defaults to
    /// `true`: installing `claude` on the host is the opt-in, and on a
    /// host without it the flag grants nothing because the PATH probe
    /// finds nothing to register.
    ///
    /// Set `false` (or run `baybo external-agent disable`) to withhold
    /// a backend that *is* installed — worth knowing that an external
    /// agent runs its own tool loop with approvals bypassed (`claude
    /// --permission-mode bypassPermissions`), so it does NOT go through
    /// baybo's sandbox / `sensitive_paths` / approval gate.
    pub enabled: bool,
    /// Path to the `claude` binary. `None` falls back to `PATH`
    /// lookup. `baybo setup` / `baybo external-agent setup` record the
    /// resolved absolute path here so the gateway service — which may
    /// run with a different cwd and a narrower `PATH` — pins the same
    /// binary the operator probed. Only consulted when `enabled = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: ENABLED_BY_DEFAULT,
            binary_path: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    /// Whether boot should probe + register this agent. See the
    /// `ClaudeConfig::enabled` docstring for rationale.
    pub enabled: bool,
    /// Path to the `codex` binary. See `ClaudeConfig::binary_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            enabled: ENABLED_BY_DEFAULT,
            binary_path: None,
        }
    }
}

impl ExternalAgentsConfig {
    /// Which kinds are switched on. Boot probes / registers only
    /// these; `baybo external-agent disable` offers exactly this set.
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
    /// path feeds this to `baybo_agent::external_agent::build_registry`
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
