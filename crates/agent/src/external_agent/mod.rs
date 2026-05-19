//! External-agent backend for subagents — see
//! `docs/external-agents.md` for the full design.
//!
//! One-shot request-response shape: each `run()` invocation drives the
//! agent through a single task and returns a stream of events
//! terminating in `FinalContent` (+ optionally `Usage` /
//! `ResumeKey`). The spawn router calls `run()` once per
//! `spawn_subagent` with `backend: external`, pipes events to the
//! parent, and stores any emitted `resume_key` on the child
//! `Session.state.subagent_backend.External.resume_key`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_llm::TokenUsage;
use aura_model::{ContentBlock, ExternalAgentKind};
use futures::Stream;
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

#[derive(Debug, Clone)]
pub enum ExternalAgentEvent {
    /// Incremental assistant text. The spawn router can forward these
    /// to the parent as `AgentOutput::Delta` for live progress.
    TextDelta(String),
    /// Resume-key the parent's next spawn can pass back via
    /// `ExternalAgentRequest.resume_key`. The spawn router stores
    /// it on `Session.state.subagent_backend.External.resume_key`.
    ResumeKey(String),
    /// Token usage for cost-records / observability. Subscription-
    /// billed agents (claude code Max, codex on ChatGPT Plus) report
    /// it on the final result event.
    Usage(TokenUsage),
    /// Terminal event — the agent has finished. Emitted exactly once,
    /// as the last item before the stream closes.
    FinalContent(Vec<ContentBlock>),
}

pub type ExternalAgentStream =
    Pin<Box<dyn Stream<Item = Result<ExternalAgentEvent>> + Send>>;

#[async_trait]
pub trait ExternalAgent: Send + Sync {
    fn kind(&self) -> ExternalAgentKind;

    async fn run(&self, request: ExternalAgentRequest) -> Result<ExternalAgentStream>;
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
    use futures::stream;

    struct StaticAgent {
        kind: ExternalAgentKind,
    }

    #[async_trait]
    impl ExternalAgent for StaticAgent {
        fn kind(&self) -> ExternalAgentKind {
            self.kind
        }
        async fn run(&self, _request: ExternalAgentRequest) -> Result<ExternalAgentStream> {
            Ok(Box::pin(stream::iter(vec![Ok(
                ExternalAgentEvent::FinalContent(vec![ContentBlock::Text("ok".into())]),
            )])))
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
    async fn registered_agent_run_emits_final_content() {
        use futures::StreamExt;
        let mut reg = ExternalAgentRegistry::new();
        reg.register(Arc::new(StaticAgent {
            kind: ExternalAgentKind::Claude,
        }));
        let agent = reg.get(ExternalAgentKind::Claude).unwrap();
        let mut stream = agent
            .run(ExternalAgentRequest {
                task: "hi".into(),
                workspace_dir: PathBuf::from("/tmp"),
                resume_key: None,
                cancel: CancellationToken::new(),
                timeout: Duration::from_secs(5),
            })
            .await
            .unwrap();
        let event = stream.next().await.unwrap().unwrap();
        match event {
            ExternalAgentEvent::FinalContent(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::Text(t) => assert_eq!(t, "ok"),
                    other => panic!("unexpected block: {other:?}"),
                }
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
