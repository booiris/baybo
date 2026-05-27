//! End-to-end harness that spawns a real `AgentActor` wired up with the
//! shared in-memory fixtures.
//!
//! Tests use [`AgentTestHarnessBuilder`] to construct an actor whose
//! mailbox they can push `IncomingMessage`s into and whose `AgentOutput`
//! receiver they can drain. The stub LLM, security gateway, secret
//! vault, and observability stores are all exposed so assertions can
//! reach into post-run state without re-wiring anything.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aura_agent::{
    AgentLoop, SecurityGateway,
    actor::{AgentActor, AgentMessage, mailbox::MailboxSender},
    tool_executor::ToolExecutor,
};
use aura_channels::{AgentEvent, AgentOutput, IncomingMessage, Message};
use aura_context::{ContextManager, ContextManagerConfig, TiktokenTokenizer};
use aura_cost::test_support::MemoryCostStore;
use aura_cost::{CostManager, SpendingLimits, cost_hooks};
use aura_job::JobLifecycle;
use aura_job::test_support::MemoryJobStore;
use aura_llm::test_support::StubLlm;
use aura_llm::{LlmCompletion, ModelPricing};
use aura_model::{ChannelType, ContentBlock, LlmEntryName, MessageMetadata, Session, User};
use aura_security::test_support::MemorySecretStore;
use aura_security::{LeakDetector, SecretVault};
use aura_skills::SkillRegistry;
use aura_tools::{ApprovalGateMap, Tool, ToolManifest, ToolRegistry};
use aura_trace::test_support::MemoryTraceStore;
use aura_trace::{SpanRecorder, TraceEventStream, TraceStore};
use chrono::Utc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::fixtures::{SessionBuilder, master_key_for_tests};

/// Test harness wrapping a spawned `AgentActor`.
///
/// Construct with [`AgentTestHarnessBuilder`]. After `build()`:
/// 1. Push canned `LlmResponse` / stream events onto `stub_llm` for the
///    upcoming turn.
/// 2. Optionally register tools onto `tool_registry` (use `EchoTool` /
///    `RecordingTool` from `aura_tools::test_support`).
/// 3. Send user input via [`AgentTestHarness::send_text`] or
///    [`AgentTestHarness::send_message`].
/// 4. Drain channel output via [`AgentTestHarness::drain_outputs`].
/// 5. Call [`AgentTestHarness::shutdown`] to stop the actor cleanly.
pub struct AgentTestHarness {
    pub session: Session,
    pub stub_llm: Arc<StubLlm>,
    pub gateway: Arc<SecurityGateway>,
    pub vault: Arc<SecretVault>,
    pub secret_store: Arc<MemorySecretStore>,
    pub job_store: Arc<MemoryJobStore>,
    pub cost_store: Arc<MemoryCostStore>,
    pub trace_store: Arc<MemoryTraceStore>,
    pub tool_registry: Arc<ToolRegistry>,
    pub skill_registry: Arc<SkillRegistry>,
    pub cost_manager: Arc<CostManager>,
    pub token_calibration: Arc<aura_context::TokenCalibration>,
    /// Session manager backing the wired `ContextManager`. Exposed so
    /// tests can pre-seed session messages or summary metadata before
    /// driving the agent loop (e.g. compressor fast-path e2e coverage).
    pub session_manager: Arc<aura_agent::SessionManager>,
    pub mailbox: MailboxSender<AgentMessage>,
    outputs: mpsc::Receiver<AgentOutput>,
    actor_handle: Option<JoinHandle<()>>,
}

impl AgentTestHarness {
    /// Start a fresh builder.
    pub fn builder() -> AgentTestHarnessBuilder {
        AgentTestHarnessBuilder::default()
    }

    /// Wrap `text` in a session-scoped `IncomingMessage`, run it through
    /// `SecurityGateway::sanitize_input` (the same pre-actor step the
    /// real `Router` performs), then dispatch onto the actor mailbox.
    ///
    /// Sanitization happens against the harness's shadow `Session`. The
    /// vault is `Arc`-shared with the actor, so any minted secret is
    /// observable through `harness.secret_store` regardless of which
    /// session owns the placeholder map.
    pub async fn send_text(&mut self, text: impl Into<String>) -> anyhow::Result<()> {
        let message = Message {
            id: format!("msg-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)),
            session_id: self.session.id.clone(),
            channel: self.session.channel.clone(),
            sender: self.session.user.clone(),
            content: vec![ContentBlock::Text(text.into())],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        };
        self.send_message(IncomingMessage {
            message,
            platform_msg_id: String::new(),
        })
        .await
    }

    /// Sanitize and push an arbitrary `IncomingMessage` onto the actor
    /// mailbox. See [`AgentTestHarness::send_text`] for the rationale on
    /// pre-sanitization.
    pub async fn send_message(&mut self, mut incoming: IncomingMessage) -> anyhow::Result<()> {
        self.gateway
            .sanitize_input(&mut incoming.message, &mut self.session)
            .await
            .map_err(|e| anyhow::anyhow!("sanitize_input failed: {e}"))?;
        self.mailbox
            .send(AgentMessage::UserInput(Box::new(incoming)))
            .await
            .map_err(|_| anyhow::anyhow!("actor mailbox closed"))?;
        Ok(())
    }

    /// Drain every `AgentOutput` that arrives within `timeout`. Returns
    /// once `timeout` elapses with no new event (so callers don't have
    /// to predict how many messages will arrive).
    pub async fn drain_outputs(&mut self, timeout: Duration) -> Vec<AgentOutput> {
        let mut out = Vec::new();
        while let Ok(Some(ev)) = tokio::time::timeout(timeout, self.outputs.recv()).await {
            out.push(ev);
        }
        out
    }

    /// Concatenate every `AnswerDelta` text in arrival order. Convenient for
    /// asserting placeholder integrity across streamed chunks.
    pub fn delta_text(outputs: &[AgentOutput]) -> String {
        outputs
            .iter()
            .filter_map(|o| match o {
                AgentOutput {
                    event: AgentEvent::AnswerDelta(text),
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Send `Shutdown` and await actor termination. Idempotent — calling
    /// twice on the same harness is safe.
    pub async fn shutdown(mut self) {
        let _ = self.mailbox.send(AgentMessage::ActorStop).await;
        if let Some(handle) = self.actor_handle.take() {
            let _ = handle.await;
        }
    }
}

/// Builder for [`AgentTestHarness`].
///
/// Defaults: a `Session` with `id = "sess-it"` on `ChannelType::tui()`,
/// the standard `LeakDetector::with_default_rules()` security stack
/// pointed at an in-memory `SecretStore`, an empty `ToolRegistry` and
/// `SkillRegistry`, the soul `"You are Aura, a test assistant."`, and
/// `max_iterations = 20`. Mailbox capacity defaults to 32.
pub struct AgentTestHarnessBuilder {
    session: Option<Session>,
    soul_prompt: Option<String>,
    mailbox_capacity: usize,
    output_capacity: usize,
    tools: Vec<(Arc<dyn Tool>, ToolManifest)>,
    spending_limits: SpendingLimits,
    pricing: HashMap<String, ModelPricing>,
    workspace: Option<Arc<aura_workspace::WorkspacePaths>>,
    keep_recent: Option<usize>,
    /// Override for the stub LLM's `context_window`, which becomes the
    /// `ContextManager`'s budget cap after `AgentLoop::from_config`
    /// installs it. Defaults to the stub's built-in 8_192.
    model_context_window: Option<usize>,
    /// Compression threshold applied to the `ContextManager` budget.
    /// Defaults to `0.95` — loose enough that tests not exercising
    /// compression stay under it with the stub's default 8_192-token
    /// window.
    compression_threshold: Option<f64>,
}

impl Default for AgentTestHarnessBuilder {
    fn default() -> Self {
        Self {
            session: None,
            soul_prompt: None,
            mailbox_capacity: 32,
            output_capacity: 64,
            tools: Vec::new(),
            spending_limits: SpendingLimits::default(),
            pricing: HashMap::new(),
            workspace: None,
            keep_recent: None,
            model_context_window: None,
            compression_threshold: None,
        }
    }
}

impl AgentTestHarnessBuilder {
    pub fn session(mut self, session: Session) -> Self {
        self.session = Some(session);
        self
    }

    pub fn soul_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.soul_prompt = Some(prompt.into());
        self
    }

    pub fn mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    pub fn output_capacity(mut self, capacity: usize) -> Self {
        self.output_capacity = capacity;
        self
    }

    /// Register a tool that the agent can invoke. Tools are registered
    /// in declaration order. The same `Arc` is handed to the harness so
    /// callers can keep a clone (e.g. a `RecordingTool`) for assertions.
    pub fn with_tool(mut self, tool: Arc<dyn Tool>, manifest: ToolManifest) -> Self {
        self.tools.push((tool, manifest));
        self
    }

    /// Override the harness's `SpendingLimits` (default: all `None`).
    /// Use to write tests that exercise budget gates.
    pub fn with_spending_limits(mut self, limits: SpendingLimits) -> Self {
        self.spending_limits = limits;
        self
    }

    /// Override the pricing map handed to `CostManager` (default: empty).
    /// Pair with a `SpendingLimits` cap to make `record_call`-driven
    /// state visible to budget gates.
    pub fn with_pricing(mut self, pricing: HashMap<String, ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }

    /// Override the workspace handle the `ContextManager` resolves
    /// per-session paths through (`summary.md`, JSONL transcript).
    /// Default: a workspace rooted at a non-existent path so the
    /// fast-path always falls through.
    pub fn with_workspace(mut self, workspace: Arc<aura_workspace::WorkspacePaths>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Override `keep_recent` (default: 50).
    pub fn with_keep_recent(mut self, keep_recent: usize) -> Self {
        self.keep_recent = Some(keep_recent);
        self
    }

    /// Override the stub LLM's `context_window`. The
    /// `ContextManager` budget cap is sourced from this value because
    /// `AgentLoop::from_config` installs it via
    /// `ContextManager::set_active_model_context_window`. Use a small
    /// value to force compression with short canned messages.
    pub fn with_model_context_window(mut self, window: usize) -> Self {
        self.model_context_window = Some(window);
        self
    }

    /// Override the compression threshold applied to the
    /// `ContextManager` budget. Defaults to `0.95`.
    pub fn with_compression_threshold(mut self, threshold: f64) -> Self {
        self.compression_threshold = Some(threshold);
        self
    }

    /// Wire everything and spawn the `AgentActor`. The returned harness
    /// owns the mailbox sender and the response receiver.
    pub fn build(self) -> AgentTestHarness {
        let session = self
            .session
            .unwrap_or_else(|| SessionBuilder::new().build());

        // Security stack reuses the same fixtures as `gateway_with_memory_vault`
        // but keeps the concrete in-memory store handle so tests can
        // assert on vault state.
        let detector = Arc::new(LeakDetector::with_default_rules());
        let secret_store = Arc::new(MemorySecretStore::new());
        let vault = Arc::new(SecretVault::new(
            master_key_for_tests(),
            secret_store.clone() as Arc<dyn aura_store::SecretStore>,
        ));
        let gateway = Arc::new(SecurityGateway::new(detector, vault.clone()));

        // Observability stores.
        let job_store = Arc::new(MemoryJobStore::new());
        let cost_store = Arc::new(MemoryCostStore::new());
        let trace_store = Arc::new(MemoryTraceStore::new());

        let job_lifecycle = Arc::new(JobLifecycle::new(share_job_store(&job_store)));
        // One shared bus per harness — the recorder publishes into it
        // and the cost subscriber drains the same bus. Constructing it
        // upfront keeps both sides explicit about which stream they
        // share (forgetting this is the silent under-billing bug
        // SpanRecorder::new now refuses to compile around).
        let trace_event_stream = TraceEventStream::new();
        let span_recorder = Arc::new(SpanRecorder::new(
            session.id.clone(),
            session.user.id.clone(),
            trace_store.clone() as Arc<dyn TraceStore>,
            trace_event_stream.clone(),
        ));
        // Cost ledger. Default builder leaves pricing empty (cost_usd
        // resolves to 0) and limits as `None` (no budget gate). Tests
        // that exercise budgeting use `with_pricing` / `with_spending_limits`.
        let cost_manager = CostManager::new(
            share_cost_store(&cost_store),
            self.pricing,
            self.spending_limits,
        );

        // Agent loop dependencies.
        let stub_llm = {
            let mut stub = StubLlm::new();
            if let Some(window) = self.model_context_window {
                let mut info = stub.model_info().clone();
                info.context_window = window;
                stub = stub.with_model_info(info);
            }
            Arc::new(stub)
        };
        let mut tool_registry = ToolRegistry::new();
        for (tool, manifest) in self.tools {
            tool_registry.register(tool, manifest);
        }
        let tool_registry = Arc::new(tool_registry);
        let skill_registry = Arc::new(SkillRegistry::new());
        let approval_gates = Arc::new(ApprovalGateMap::new());
        let tool_executor = Arc::new(ToolExecutor::new(
            tool_registry.clone(),
            approval_gates,
            gateway.clone(),
            std::path::PathBuf::from("/tmp"),
            aura_workspace::WorkspacePaths::new(std::path::PathBuf::from("/tmp")),
            None,
        ));

        // Tokenizer model id must match the LLM client's so
        // `TokenCalibration` keys observe and adjust identically.
        let stub_model_id = stub_llm.model_info().id.clone();
        let tokenizer = Arc::new(TiktokenTokenizer::for_model(&stub_model_id));
        // Non-existent root → fast-path read hits NotFound → falls
        // through. No tempdir to clean up.
        let workspace = self.workspace.unwrap_or_else(|| {
            Arc::new(aura_workspace::WorkspacePaths::new(PathBuf::from(
                "/nonexistent-aura-it-workspace",
            )))
        });
        let keep_recent = self.keep_recent.unwrap_or(50);
        let compression_threshold = self.compression_threshold.unwrap_or(0.95);
        let token_calibration = Arc::new(aura_context::TokenCalibration::new());
        let session_store = Arc::new(aura_session::test_support::MemorySessionStore::new())
            as Arc<dyn aura_session::SessionStore>;
        let summary_store = Arc::new(aura_session::test_support::MemorySessionSummaryStore::new())
            as Arc<dyn aura_session::SessionSummaryStore>;
        let session_manager = Arc::new(aura_agent::SessionManager::new(
            session_store,
            summary_store,
        ));
        let soul_text = self
            .soul_prompt
            .unwrap_or_else(|| "You are Aura, a test assistant.".into());
        // Inject the fixed test prompt as a one-profile subagent registry: the
        // new design resolves a session's system prompt from either the
        // workspace soul or a subagent profile, and the harness workspace is a
        // stub (so the workspace path would just hit the fallback).
        let subagent_registry = Arc::new(aura_subagent::SubagentRegistry::new());
        subagent_registry.register(aura_subagent::SubagentProfile {
            name: "harness".into(),
            version: "1".into(),
            description: "harness test prompt".into(),
            system_prompt: soul_text,
            default_tier: None,
            source: aura_model::ArtifactSource::Inline,
            trust_level: aura_model::TrustLevel::Trusted,
            source_path: None,
        });
        let context_manager = ContextManager::from_config(ContextManagerConfig {
            tokenizer,
            workspace,
            keep_recent,
            compression_threshold,
            calibration: Arc::clone(&token_calibration),
            skill_registry: Arc::clone(&skill_registry),
            session_id: session.id.clone(),
            sessions: Arc::clone(&session_manager),
            subagent_profile: Some((subagent_registry, "harness".to_string())),
            session_log: None,
        });

        let guarded_llm = aura_llm::BillableLlm::new(
            stub_llm.clone() as Arc<dyn aura_llm::LlmCompletion>,
            cost_hooks(&cost_manager),
        );

        // Single-entry pool keyed by the stub model's id; tests that
        // exercise mid-session swap can extend this by registering
        // extra stubs under different names via a builder method.
        let stub_entry_name = LlmEntryName::from(stub_llm.model_info().id.clone());
        let mut pool_clients = std::collections::HashMap::new();
        pool_clients.insert(stub_entry_name.clone(), guarded_llm);
        let llm_pool: aura_agent::LlmPoolHandle = Arc::new(parking_lot::RwLock::new(Arc::new(
            aura_agent::LlmClientPool::new(pool_clients, stub_entry_name.clone())
                .expect("stub pool default present"),
        )));

        let actor_parent_token = tokio_util::sync::CancellationToken::new();
        let actor_token = actor_parent_token.child_token();

        let agent_loop = AgentLoop::from_config(aura_agent::agent_loop::AgentLoopConfig {
            llm_pool,
            initial_llm: None,
            tool_registry: tool_registry.clone(),
            tool_executor: tool_executor.clone(),
            context_manager,
            max_iterations: 20,
            security_gateway: gateway.clone(),
            actor_token: actor_token.clone(),
            system_spawn_tx: None,
            workspace_paths: None,
            sessions: None,
            memory: None,
        });
        let (mailbox_tx, mailbox_rx) = aura_agent::mailbox::channel(self.mailbox_capacity);
        let (output_tx, output_rx) = mpsc::channel(self.output_capacity);

        let actor = AgentActor::from_parts(
            aura_agent::state::DurableActorState::new(session.clone()),
            aura_agent::state::VolatileResources {
                agent_loop,
                response_tx: output_tx,
                job_lifecycle,
                span_recorder,
                actor_token,
                supervisor: None,
                session_manager: Arc::clone(&session_manager),
            },
        );
        let actor_handle = tokio::spawn(actor.run(mailbox_rx));

        AgentTestHarness {
            session,
            stub_llm,
            gateway,
            vault,
            secret_store,
            job_store,
            cost_store,
            trace_store,
            tool_registry,
            skill_registry,
            cost_manager,
            token_calibration,
            session_manager,
            mailbox: mailbox_tx,
            outputs: output_rx,
            actor_handle: Some(actor_handle),
        }
    }
}

// --- Helpers to obtain `Box<dyn Trait>` handles from `Arc<Concrete>` ---
//
// Each in-memory store is constructed once as an `Arc` and shared
// directly with the managers — `JobLifecycle` and `CostManager` accept
// `Arc<dyn Trait>`, so the test handle and the manager-owned handle
// point at the same instance and post-run assertions see real state.

fn share_job_store(arc: &Arc<MemoryJobStore>) -> Arc<dyn aura_job::JobStore> {
    arc.clone()
}

fn share_cost_store(arc: &Arc<MemoryCostStore>) -> Arc<dyn aura_cost::CostStore> {
    arc.clone()
}

// Avoid unused-import lints when callers don't reference these types.
#[allow(dead_code)]
fn _typecheck_session(_: &User, _: &ChannelType) {}
