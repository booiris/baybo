//! End-to-end harness that spawns a real `AgentActor` wired up with the
//! shared in-memory fixtures.
//!
//! Tests use [`AgentTestHarnessBuilder`] to construct an actor whose
//! mailbox they can push `IncomingMessage`s into and whose `AgentOutput`
//! receiver they can drain. The stub LLM, security gateway, secret
//! vault, and observability stores are all exposed so assertions can
//! reach into post-run state without re-wiring anything.

use std::sync::Arc;
use std::time::Duration;

use aura_agent::{
    AgentLoop, ExecutionPolicy, JobLifecycle, MemoryManager, SecurityGateway, SpanRecorder,
    actor::{AgentActor, AgentMessage},
    soul::Soul,
    tool_executor::ToolExecutor,
};
use aura_channels::{AgentOutput, IncomingMessage, Message};
use aura_context::{ContextManager, TiktokenTokenizer, Truncate, budget::TokenBudget};
use aura_llm::test_support::StubLlm;
use aura_model::{ChannelType, ContentBlock, MessageMetadata, Session, User};
use aura_security::{LeakDetector, SecretVault};
use aura_skills::SkillRegistry;
use aura_storage::test_support::{
    MemoryCostStore, MemoryJobStore, MemoryMemoryStore, MemorySecretStore, MemoryTraceStore,
};
use aura_tools::{ApprovalGateMap, Tool, ToolManifest, ToolRegistry};
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
    pub memory_store: Arc<MemoryMemoryStore>,
    pub tool_registry: Arc<ToolRegistry>,
    pub skill_registry: Arc<SkillRegistry>,
    pub mailbox: mpsc::Sender<AgentMessage>,
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
            session_id: self.session.id.to_string(),
            channel: self.session.channel.clone(),
            sender: self.session.user.clone(),
            content: vec![ContentBlock::Text(text.into())],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        };
        self.send_message(IncomingMessage { message }).await
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

    /// Concatenate every `Delta` text in arrival order. Convenient for
    /// asserting placeholder integrity across streamed chunks.
    pub fn delta_text(outputs: &[AgentOutput]) -> String {
        outputs
            .iter()
            .filter_map(|o| match o {
                AgentOutput::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Send `Shutdown` and await actor termination. Idempotent — calling
    /// twice on the same harness is safe.
    pub async fn shutdown(mut self) {
        let _ = self.mailbox.send(AgentMessage::Shutdown).await;
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
/// `ExecutionPolicy::default()`. Mailbox capacity defaults to 32.
pub struct AgentTestHarnessBuilder {
    session: Option<Session>,
    soul_prompt: Option<String>,
    mailbox_capacity: usize,
    output_capacity: usize,
    tools: Vec<(Arc<dyn Tool>, ToolManifest)>,
}

impl Default for AgentTestHarnessBuilder {
    fn default() -> Self {
        Self {
            session: None,
            soul_prompt: None,
            mailbox_capacity: 32,
            output_capacity: 64,
            tools: Vec::new(),
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
            secret_store.clone() as Arc<dyn aura_storage::SecretStore>,
        ));
        let spill_dir = std::env::temp_dir().join(format!(
            "aura-it-tool-spills-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let gateway =
            Arc::new(SecurityGateway::new(detector, vault.clone()).with_spill_dir(spill_dir));

        // Observability stores.
        let job_store = Arc::new(MemoryJobStore::new());
        let cost_store = Arc::new(MemoryCostStore::new());
        let trace_store = Arc::new(MemoryTraceStore::new());
        let memory_store = Arc::new(MemoryMemoryStore::new());

        let job_lifecycle = Arc::new(JobLifecycle::new(share_job_store(&job_store)));
        // One shared bus per harness — the recorder publishes into it
        // and the cost subscriber drains the same bus. Constructing it
        // upfront keeps both sides explicit about which stream they
        // share (forgetting this is the silent under-billing bug
        // SpanRecorder::new now refuses to compile around).
        let trace_event_stream = aura_agent::TraceEventStream::new();
        let span_recorder = Arc::new(SpanRecorder::new(
            session.id.clone(),
            session.user.id.clone(),
            trace_store.clone() as Arc<dyn aura_storage::TraceStore>,
            trace_event_stream.clone(),
        ));
        // Cost subscriber: pricing left empty for tests — token
        // counts still land in cost_records, cost_usd reads as 0.
        let _cost_handle = aura_agent::cost::CostSubscriber::new(
            share_cost_store(&cost_store),
            Arc::new(std::collections::HashMap::new()),
        )
        .spawn(&trace_event_stream);

        // Agent loop dependencies.
        let stub_llm = Arc::new(StubLlm::new());
        let mut tool_registry = ToolRegistry::new();
        for (tool, manifest) in self.tools {
            tool_registry.register(tool, manifest);
        }
        let tool_registry = Arc::new(tool_registry);
        let skill_registry = Arc::new(SkillRegistry::new());
        let memory_manager = Arc::new(MemoryManager::without_embedder(share_memory_store(
            &memory_store,
        )));
        let approval_gates = Arc::new(ApprovalGateMap::new());
        let tool_executor = Arc::new(ToolExecutor::new(
            tool_registry.clone(),
            Duration::from_secs(5),
            approval_gates,
            gateway.clone(),
            std::path::PathBuf::from("/tmp"),
            None,
        ));

        let tokenizer = Arc::new(TiktokenTokenizer::default());
        let context_manager = ContextManager::new(
            tokenizer,
            Box::new(Truncate::new(50)),
            TokenBudget::new(100_000, 0.95),
        );

        let soul_text = self
            .soul_prompt
            .unwrap_or_else(|| "You are Aura, a test assistant.".into());
        let soul = Soul::custom(soul_text);

        let agent_loop = AgentLoop::new(
            stub_llm.clone() as Arc<dyn aura_llm::LlmCompletion>,
            tool_registry.clone(),
            skill_registry.clone(),
            tool_executor.clone(),
            context_manager,
            memory_manager,
            ExecutionPolicy::default(),
            soul,
            gateway.clone(),
        );
        let (mailbox_tx, mailbox_rx) = mpsc::channel(self.mailbox_capacity);
        let (output_tx, output_rx) = mpsc::channel(self.output_capacity);

        let actor_parent_token = tokio_util::sync::CancellationToken::new();
        let actor = AgentActor::new(
            session.clone(),
            agent_loop,
            output_tx,
            job_lifecycle,
            span_recorder,
            &actor_parent_token,
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
            memory_store,
            tool_registry,
            skill_registry,
            mailbox: mailbox_tx,
            outputs: output_rx,
            actor_handle: Some(actor_handle),
        }
    }
}

// --- Helpers to obtain `Box<dyn Trait>` handles from `Arc<Concrete>` ---
//
// Each in-memory store is constructed once as an `Arc` and shared
// directly with the managers — `JobLifecycle`, `CostSubscriber`, and
// `MemoryManager` all accept `Arc<dyn Trait>`, so the test handle and the
// manager-owned handle point at the same instance and post-run
// assertions see real state.

fn share_job_store(arc: &Arc<MemoryJobStore>) -> Arc<dyn aura_storage::JobStore> {
    arc.clone()
}

fn share_cost_store(arc: &Arc<MemoryCostStore>) -> Arc<dyn aura_storage::CostStore> {
    arc.clone()
}

fn share_memory_store(arc: &Arc<MemoryMemoryStore>) -> Arc<dyn aura_storage::MemoryStore> {
    arc.clone()
}

// Avoid unused-import lints when callers don't reference these types.
#[allow(dead_code)]
fn _typecheck_session(_: &User, _: &ChannelType) {}
