//! External-agent backend for subagents — see
//! `docs/external-agents.md` for the full design.
//!
//! One-shot request-response shape: each `run()` invocation drives the
//! agent through a single task and returns an `ExternalAgentOutcome`
//! with the final assistant content (+ optional `resume_key` for
//! continuation, optional `usage` for cost ledger). The spawn router
//! calls `run()` once per `spawn_subagent` with `backend: external`,
//! persists any emitted `resume_key` on the child
//! `Session.state.subagent_backend.External.resume_key`, and forwards
//! the final content to the parent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_llm::TokenUsage;
use aura_model::{ContentBlock, ExternalAgentKind};
use tokio_util::sync::CancellationToken;

pub mod claude_cli;
pub mod codex_cli;
mod probe;

pub type Result<T> = std::result::Result<T, ExternalAgentError>;

#[derive(Debug, thiserror::Error)]
pub enum ExternalAgentError {
    /// Binary not found in PATH and no explicit path configured.
    /// Split from `Config` so the CLI / status command can label
    /// "not installed" distinctly from other operator-action
    /// failures (login, broken binary, …).
    #[error("external agent: {0}")]
    NotInstalled(String),
    /// Operator must act (binary present but broken, not logged in,
    /// malformed config).
    #[error("external agent: {0}")]
    Config(String),
    /// Recoverable failure (rate limit, transient I/O).
    #[error("external agent: {0}")]
    Transient(String),
}

#[derive(Debug, Clone)]
pub struct ExternalAgentRequest {
    pub task: String,
    /// Working directory for the agent's filesystem activity. Caller
    /// guarantees it exists and is writable. Each spawn gets its own
    /// dir under `<workspace_root>/work/<kind>/<dir>/`.
    pub workspace_dir: PathBuf,
    /// Opaque continuation pointer from a prior call's
    /// `ResumeKey` event. `None` on first spawn.
    pub resume_key: Option<String>,
    pub cancel: CancellationToken,
    pub timeout: Duration,
}

/// Final state of one `ExternalAgent::run()` call.
#[derive(Debug, Clone, Default)]
pub struct ExternalAgentOutcome {
    /// Assistant text the agent produced. Empty if the agent finished
    /// without emitting any user-facing content.
    pub final_content: Vec<ContentBlock>,
    /// Continuation pointer the agent emitted on a *fresh* run
    /// (`request.resume_key` was `None`). Stored on
    /// `Session.state.subagent_backend.External.resume_key`. Resume
    /// runs preserve the prior key; this field is `None` for them.
    pub resume_key: Option<String>,
    /// Token usage if the agent reported it. Optional — subscription-
    /// billed agents (claude code Max, codex on ChatGPT Plus) don't
    /// always emit it.
    pub usage: Option<TokenUsage>,
}

#[async_trait]
pub trait ExternalAgent: Send + Sync {
    fn kind(&self) -> ExternalAgentKind;

    async fn run(&self, request: ExternalAgentRequest) -> Result<ExternalAgentOutcome>;
}

#[derive(Default)]
pub struct ExternalAgentRegistry {
    agents: HashMap<ExternalAgentKind, Arc<dyn ExternalAgent>>,
}

impl ExternalAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, agent: Arc<dyn ExternalAgent>) {
        self.agents.insert(agent.kind(), agent);
    }

    pub fn get(&self, kind: ExternalAgentKind) -> Option<Arc<dyn ExternalAgent>> {
        self.agents.get(&kind).cloned()
    }

    pub fn registered(&self) -> Vec<ExternalAgentKind> {
        self.agents.keys().copied().collect()
    }
}

/// Probe + register every entry marked `enabled = true`. Disabled
/// kinds are skipped silently; probe failure on an enabled kind logs
/// `warn!` and continues (operator misconfiguration doesn't block
/// boot). Adding a new `ExternalAgentKind` only requires extending
/// the inner match here — boot paths just pass through their per-
/// kind config (see `aura_config::ExternalAgentsConfig::boot_entries`).
///
/// Tuple form `(kind, enabled, binary_path)` is the lingua franca
/// between this crate and `aura-config` since the two don't depend on
/// each other.
pub fn build_registry<'a, I>(entries: I) -> ExternalAgentRegistry
where
    I: IntoIterator<Item = (ExternalAgentKind, bool, Option<&'a str>)>,
{
    let mut registry = ExternalAgentRegistry::new();
    for (kind, enabled, binary_path) in entries {
        if !enabled {
            tracing::debug!(
                kind = kind.as_str(),
                "external agent disabled in config; not probing",
            );
            continue;
        }
        let result: Result<Arc<dyn ExternalAgent>> = match kind {
            ExternalAgentKind::Claude => claude_cli::ClaudeCliAgent::probe_and_build(binary_path)
                .map(|a| a as Arc<dyn ExternalAgent>),
            ExternalAgentKind::Codex => codex_cli::CodexCliAgent::probe_and_build(binary_path)
                .map(|a| a as Arc<dyn ExternalAgent>),
        };
        match result {
            Ok(agent) => {
                tracing::info!(kind = kind.as_str(), "external agent registered");
                registry.register(agent);
            }
            Err(e) => {
                tracing::warn!(
                    kind = kind.as_str(),
                    error = %e,
                    "external agent enabled but probe failed; spawn_subagent(backend: <kind>) calls for this kind will fail until resolved",
                );
            }
        }
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticAgent {
        kind: ExternalAgentKind,
    }

    #[async_trait]
    impl ExternalAgent for StaticAgent {
        fn kind(&self) -> ExternalAgentKind {
            self.kind
        }
        async fn run(&self, _request: ExternalAgentRequest) -> Result<ExternalAgentOutcome> {
            Ok(ExternalAgentOutcome {
                final_content: vec![ContentBlock::Text("ok".into())],
                resume_key: None,
                usage: None,
            })
        }
    }

    #[test]
    fn registry_register_and_lookup_round_trip() {
        let mut reg = ExternalAgentRegistry::new();
        reg.register(Arc::new(StaticAgent {
            kind: ExternalAgentKind::Claude,
        }));
        assert!(reg.get(ExternalAgentKind::Claude).is_some());
        assert_eq!(reg.registered(), vec![ExternalAgentKind::Claude]);
    }

    #[tokio::test]
    async fn registered_agent_run_returns_final_content() {
        let mut reg = ExternalAgentRegistry::new();
        reg.register(Arc::new(StaticAgent {
            kind: ExternalAgentKind::Claude,
        }));
        let agent = reg.get(ExternalAgentKind::Claude).unwrap();
        let outcome = agent
            .run(ExternalAgentRequest {
                task: "hi".into(),
                workspace_dir: PathBuf::from("/tmp"),
                resume_key: None,
                cancel: CancellationToken::new(),
                timeout: Duration::from_secs(5),
            })
            .await
            .unwrap();
        assert_eq!(outcome.final_content.len(), 1);
        match &outcome.final_content[0] {
            ContentBlock::Text(t) => assert_eq!(t, "ok"),
            other => panic!("unexpected block: {other:?}"),
        }
        assert!(outcome.resume_key.is_none());
        assert!(outcome.usage.is_none());
    }
}
