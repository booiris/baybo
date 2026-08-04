//! Domain types for external-agent-backed subagents — i.e. sub-agents
//! whose work is delegated to a subprocess (or other out-of-process
//! driver) instead of running on an in-process baybo `AgentActor`.
//!
//! External agents are request-response: one task in, one final result
//! out (their internal tool loop is invisible to the parent).
//! Cross-call continuity is via a `resume_key` that the external agent
//! emits; the spawn router stores it on
//! `Session.state.subagent_backend.External.resume_key`.

use serde::{Deserialize, Serialize};

/// Discriminator for the external-agent implementations registered at
/// boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAgentKind {
    Claude,
    Codex,
}

impl ExternalAgentKind {
    pub const ALL: &'static [ExternalAgentKind] = &[Self::Claude, Self::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Human-readable label (for CLI pickers / status output).
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code (claude)",
            Self::Codex => "Codex (codex)",
        }
    }

    /// PATH-lookup binary name.
    pub fn binary_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// Which backend the spawn router should use for a `SubagentSpawnRequest`.
///
/// `Baybo` is the default and matches today's behaviour: spawn a full
/// in-process `AgentActor` (multi-turn, transcript, skills, tools).
///
/// `External` is one-shot and routes to the matching `ExternalAgent`
/// impl registered at boot. No `AgentActor` is built; the agent's
/// stream of events is piped straight back to the parent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentBackend {
    #[default]
    Baybo,
    External {
        external_kind: ExternalAgentKind,
    },
}

impl SubagentBackend {
    /// Discriminator for runtime dispatch. `SubagentBackendTag` (the
    /// storage shape) needs operational state the request doesn't
    /// carry, so the genesis path builds the tag inside the spawn
    /// router after the child Session exists.
    pub fn kind(&self) -> SubagentBackendKind {
        match self {
            Self::Baybo => SubagentBackendKind::Baybo,
            Self::External { external_kind } => SubagentBackendKind::External(*external_kind),
        }
    }
}

/// Durable record of which backend created a subagent session. Lives
/// on `SessionState.subagent_backend`.
///
/// For `External`, also carries:
/// - `workspace_dir`: on-disk dir name under `<root>/work/<kind>/`,
///   resolved once at genesis. Immutable across resume calls so a
///   sibling subagent always lands in the same dir.
/// - `resume_key`: the agent's session pointer (e.g. claude's
///   session uuid). `None` until the agent's first turn produces
///   one. Resuming an External with `None` is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentBackendTag {
    Baybo,
    External {
        external_kind: ExternalAgentKind,
        workspace_dir: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_key: Option<String>,
    },
}

/// Discriminator-only view of `SubagentBackendTag` for runtime
/// decisions that don't care about per-instance state (resume
/// validation, error labels, route dispatch). The spawn router
/// uses this when asking `resolve_child_session` to mint or
/// validate a child, because the operational fields aren't known
/// (or relevant) at that call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentBackendKind {
    Baybo,
    External(ExternalAgentKind),
}

/// Stable string used for the `baybo` backend tag wherever the spawn
/// protocol's `backend` field is parsed/rendered (spawn_subagent tool
/// schema, error messages, etc.).
pub const BAYBO_BACKEND_TAG: &str = "baybo";

impl SubagentBackendKind {
    /// Short label used in error messages and the spawn protocol's
    /// `backend` field. Matches the JSON schema's enum values.
    pub fn label(self) -> &'static str {
        match self {
            Self::Baybo => BAYBO_BACKEND_TAG,
            Self::External(k) => k.as_str(),
        }
    }
}

impl SubagentBackendTag {
    /// Discriminator view — strips operational state for identity
    /// comparison.
    pub fn kind(&self) -> SubagentBackendKind {
        match self {
            Self::Baybo => SubagentBackendKind::Baybo,
            Self::External { external_kind, .. } => SubagentBackendKind::External(*external_kind),
        }
    }
}
