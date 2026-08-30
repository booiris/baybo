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

use baybo_agent::{
    AgentLoop, SecurityGateway,
    actor::{AgentActor, AgentMessage, mailbox::MailboxSender},
    tool_executor::ToolExecutor,
};
use baybo_channels::{AgentEvent, AgentOutput, IncomingMessage, Message};
use baybo_context::{ContextManager, ContextManagerConfig, TiktokenTokenizer};
use baybo_cost::test_support::MemoryCostStore;
use baybo_cost::{CostManager, SpendingLimits, cost_hooks};
use baybo_llm::test_support::StubLlm;
use baybo_llm::{LlmCompletion, ModelPricing};
use baybo_model::{ChannelType, ContentBlock, LlmEntryName, MessageMetadata, Session, User};
use baybo_security::test_support::MemorySecretStore;
use baybo_security::{LeakDetector, SecretVault};
use baybo_skills::SkillRegistry;
use baybo_tools::{ApprovalGateMap, Tool, ToolManifest, ToolRegistry};
use baybo_trace::test_support::MemoryTraceStore;
use baybo_trace::{SpanRecorder, TraceEventStream, TraceStore};
use baybo_turn::TurnLifecycle;
use baybo_turn::test_support::MemoryTurnStore;
use chrono::Utc;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::fixtures::{SessionBuilder, master_key_for_tests};

/// Test harness wrapping a spawned `AgentActor`.
///
/// Construct with [`AgentTestHarnessBuilder`]. After `build()`:
/// 1. Push canned `LlmResponse` / stream events onto `stub_llm` for the
///    upcoming turn.
/// 2. Optionally register tools onto `tool_registry` (use `EchoTool` /
///    `RecordingTool` from `baybo_tools::test_support`).
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
    pub turn_store: Arc<MemoryTurnStore>,
    /// The same `TurnLifecycle` instance the actor drives. Exposed so a test
    /// can cancel the session's in-flight turn the way the Router's `/stop`
    /// does (`list_active_chat_turns_by_session` → `cancel`), which trips the very
    /// token the agent loop threads into the live LLM call.
    pub turn_lifecycle: Arc<TurnLifecycle>,
    pub cost_store: Arc<MemoryCostStore>,
    pub trace_store: Arc<MemoryTraceStore>,
    pub tool_registry: Arc<ToolRegistry>,
    pub skill_registry: Arc<SkillRegistry>,
    pub cost_manager: Arc<CostManager>,
    pub token_calibration: Arc<baybo_context::TokenCalibration>,
    /// Session manager backing the wired `ContextManager`. Exposed so
    /// tests can pre-seed session messages or summary metadata before
    /// driving the agent loop.
    pub session_manager: Arc<baybo_agent::SessionManager>,
    /// Planning-checklist store shared by the registered `Task*` tools and the
    /// loop's per-turn reminder. Exposed so a task e2e can assert what the
    /// model's tool calls persisted.
    pub task_store: Arc<dyn baybo_store::TaskStore>,
    /// The concrete session store behind `session_manager`. Exposed so a test
    /// can inject a transcript-append failure (`fail_appends`) and drive the
    /// paths that must not treat an unpersisted row as delivered.
    pub memory_session_store: Arc<baybo_session::test_support::MemorySessionStore>,
    /// Cron delivery ledger the actor stamps when it appends a one-shot fire's
    /// result to this session. Exposed so a test can seed an execution and
    /// assert the delivery resolved it.
    pub cron_store: Arc<baybo_cron::test_support::InMemoryCronStore>,
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
            bot_id: String::new(),
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
/// `SkillRegistry`, the soul `"You are Baybo, a test assistant."`, and
/// `max_iterations = 20`. Mailbox capacity defaults to 32.
pub struct AgentTestHarnessBuilder {
    session: Option<Session>,
    soul_prompt: Option<String>,
    mailbox_capacity: usize,
    output_capacity: usize,
    tools: Vec<(Arc<dyn Tool>, ToolManifest)>,
    spending_limits: SpendingLimits,
    pricing: HashMap<String, ModelPricing>,
    workspace: Option<Arc<baybo_workspace::WorkspacePaths>>,
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
    /// Pluggable memory wired into the loop. Defaults to `None` (inert); tests
    /// assert the recall / write hooks by wiring a `RecordingMemory`.
    memory: Option<Arc<dyn baybo_memory::Memory>>,
    /// Override the LLM client wired into the pool. Defaults to `None` → the
    /// scriptable `StubLlm`. Set it to drive a custom `LlmCompletion` (e.g. a
    /// fixture whose call blocks until cancelled) when `StubLlm`'s
    /// return-immediately contract doesn't fit.
    llm: Option<Arc<dyn LlmCompletion>>,
    chat_gate: Option<(Arc<Notify>, Arc<Notify>)>,
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
            memory: None,
            llm: None,
            chat_gate: None,
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
    /// per-session paths through (today: the JSONL transcript pointer
    /// baked into a compaction's summary message). Default: a workspace
    /// rooted at a non-existent path.
    pub fn with_workspace(mut self, workspace: Arc<baybo_workspace::WorkspacePaths>) -> Self {
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

    /// Wire a [`baybo_memory::Memory`] impl into the loop so a test can assert
    /// the recall / `on_turn_complete` hooks fire. Defaults to `None` (inert).
    pub fn with_memory(mut self, memory: Arc<dyn baybo_memory::Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Wire a custom [`LlmCompletion`] into the pool instead of the default
    /// [`StubLlm`]. Use for behaviours `StubLlm` can't express — e.g. a call
    /// that blocks until its future is dropped, to test in-flight cancellation.
    pub fn with_llm(mut self, llm: Arc<dyn LlmCompletion>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Park the FIRST non-streaming `chat` (the compaction summariser is the
    /// only caller in these suites) until `release` is notified, signalling
    /// `entered` first so the test can act while the call is in flight. Later
    /// calls pass straight through, so a test can hold one compaction and then
    /// let the next one run normally.
    ///
    /// Wraps the harness's OWN stub rather than replacing it — `with_llm`
    /// swaps the client out entirely, which would silently orphan
    /// `harness.stub_llm` and every `push_*` the test made.
    pub fn with_chat_gate(mut self, entered: Arc<Notify>, release: Arc<Notify>) -> Self {
        self.chat_gate = Some((entered, release));
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
            secret_store.clone() as Arc<dyn baybo_store::SecretStore>,
        ));
        let gateway = Arc::new(SecurityGateway::new(detector, vault.clone()));

        // Observability stores.
        let turn_store = Arc::new(MemoryTurnStore::new());
        let cost_store = Arc::new(MemoryCostStore::new());
        let trace_store = Arc::new(MemoryTraceStore::new());

        let turn_lifecycle = Arc::new(TurnLifecycle::new(share_turn_store(&turn_store)));
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
        // The client actually wired into the pool: the custom override when
        // set, else the scriptable stub. Its `model_info` drives the pool
        // entry name + tokenizer so they stay consistent either way.
        let wired_llm: Arc<dyn baybo_llm::LlmCompletion> = match self.llm {
            Some(custom) => custom,
            None => match self.chat_gate {
                Some((entered, release)) => Arc::new(GatedChat {
                    inner: stub_llm.clone(),
                    entered,
                    release,
                    held: std::sync::atomic::AtomicBool::new(false),
                }),
                None => stub_llm.clone(),
            },
        };
        let mut tool_registry = ToolRegistry::new();
        for (tool, manifest) in self.tools {
            tool_registry.register(tool, manifest);
        }
        // Wire the planning-checklist tools against a shared store the loop
        // also reads for its per-turn reminder — mirrors production so a task
        // e2e exercises the full tool→store→reminder→TaskList path.
        let task_store: Arc<dyn baybo_store::TaskStore> =
            Arc::new(baybo_task::test_support::MemoryTaskStore::new());
        for (tool, manifest) in baybo_task::tools::agent_tools(task_store.clone()) {
            tool_registry.register(tool, manifest);
        }
        let tool_registry = Arc::new(tool_registry);
        let skill_registry = Arc::new(SkillRegistry::new());
        let approval_gates = Arc::new(ApprovalGateMap::new());
        let memory_session_store = Arc::new(baybo_session::test_support::MemorySessionStore::new());
        // Mirror production's "session row exists before the actor spawns"
        // shape so the transcript-read intercept's cross-session lookup finds
        // the row instead of returning NotFound.
        memory_session_store.seed_session(&session);
        let session_store =
            Arc::clone(&memory_session_store) as Arc<dyn baybo_session::SessionStore>;
        let folder_store = Arc::new(baybo_session::test_support::MemorySessionFolderStore::new())
            as Arc<dyn baybo_session::SessionFolderStore>;
        let session_manager = Arc::new(baybo_agent::SessionManager::new(
            session_store,
            folder_store,
        ));
        let virtual_reads: Option<Arc<dyn baybo_tools::VirtualReadResolver>> =
            Some(Arc::new(baybo_agent::SessionTranscriptReader::new(
                Arc::clone(&session_manager) as Arc<dyn baybo_agent::SessionTranscript>,
                baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from("/tmp")),
            )));
        let tool_executor = Arc::new(ToolExecutor::new(
            tool_registry.clone(),
            approval_gates,
            gateway.clone(),
            std::path::PathBuf::from("/tmp"),
            baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from("/tmp")),
            None,
            None,
            virtual_reads,
            None,
            None,
            None,
        ));

        // Tokenizer model id must match the LLM client's so
        // `TokenCalibration` keys observe and adjust identically.
        let stub_model_id = wired_llm.model_info().id.clone();
        let tokenizer = Arc::new(TiktokenTokenizer::for_model(&stub_model_id));
        // Non-existent root: nothing under it is read in these tests, so
        // there is no tempdir to clean up.
        let workspace = self.workspace.unwrap_or_else(|| {
            Arc::new(baybo_workspace::WorkspacePaths::new(PathBuf::from(
                "/nonexistent-baybo-it-workspace",
            )))
        });
        let keep_recent = self.keep_recent.unwrap_or(50);
        let compression_threshold = self.compression_threshold.unwrap_or(0.95);
        let token_calibration = Arc::new(baybo_context::TokenCalibration::new());
        let soul_text = self
            .soul_prompt
            .unwrap_or_else(|| "You are Baybo, a test assistant.".into());
        // Inject the fixed test prompt as a one-profile subagent registry: the
        // new design resolves a session's system prompt from either the
        // workspace soul or a subagent profile, and the harness workspace is a
        // stub (so the workspace path would just hit the fallback).
        let subagent_registry = Arc::new(baybo_subagent::SubagentRegistry::new());
        subagent_registry.register(baybo_subagent::SubagentProfile {
            name: "harness".into(),
            version: "1".into(),
            description: "harness test prompt".into(),
            system_prompt: soul_text,
            default_tier: None,
            source: baybo_model::ArtifactSource::Inline,
            trust_level: baybo_model::TrustLevel::Trusted,
            source_path: None,
            tools: None,
        });
        let context_manager = ContextManager::from_config(ContextManagerConfig {
            agent: None,
            tokenizer,
            workspace: Arc::clone(&workspace),
            keep_recent,
            compression_threshold,
            // The harness drives compaction through the window share and a
            // deliberately small stub window; an absolute cap on top would
            // be a second trigger every such test has to reason about.
            max_active_tokens: 0,
            calibration: Arc::clone(&token_calibration),
            skill_registry: Arc::clone(&skill_registry),
            channel: session.channel.clone(),
            shape: baybo_context::prompts::soul::PromptShape::for_trigger(&session.trigger),
            session_id: session.id.clone(),
            sessions: Arc::clone(&session_manager),
            subagent_profile: Some((subagent_registry, "harness".to_string())),
            builtin_memory: false,
            deferred_tool_servers: Vec::new(),
        });

        let guarded_llm =
            baybo_llm::BillableLlm::new(Arc::clone(&wired_llm), cost_hooks(&cost_manager));

        // Single-entry pool keyed by the wired model's id; tests that
        // exercise mid-session swap can extend this by registering
        // extra stubs under different names via a builder method.
        let stub_entry_name = LlmEntryName::from(wired_llm.model_info().id.clone());
        let mut pool_clients = std::collections::HashMap::new();
        pool_clients.insert(stub_entry_name.clone(), guarded_llm);
        let llm_pool: baybo_agent::LlmPoolHandle = Arc::new(parking_lot::RwLock::new(Arc::new(
            baybo_agent::LlmClientPool::new(pool_clients, stub_entry_name.clone())
                .expect("stub pool default present"),
        )));

        let actor_parent_token = tokio_util::sync::CancellationToken::new();
        let actor_token = actor_parent_token.child_token();

        let agent_loop = AgentLoop::from_config(baybo_agent::agent_loop::AgentLoopConfig {
            llm_pool,
            initial_llm: None,
            initial_model: None,
            initial_effort: None,
            tool_registry: tool_registry.clone(),
            tool_executor: tool_executor.clone(),
            context_manager,
            max_iterations: 20,
            security_gateway: gateway.clone(),
            // Mirror production for the progress observer's durable shadow and
            // title-generation paths.
            sessions: Some(Arc::clone(&session_manager)),
            memory: self.memory,
            task_store: task_store.clone(),
            title_sink: None,
        });
        let (mailbox_tx, mailbox_rx) = baybo_agent::mailbox::channel(self.mailbox_capacity);
        let (output_tx, output_rx) = mpsc::channel(self.output_capacity);

        // Mirror production's turn-state projector so the turn-*ended*
        // `TurnState { active: false }` is derived from the turn store
        // (the actor only emits the leading `true`). Without it the e2e
        // outputs would never carry the close edge.
        baybo_agent::supervisor::spawn_turn_state_projector(
            Arc::clone(&turn_lifecycle),
            Arc::clone(&session_manager),
            output_tx.clone(),
            actor_parent_token.child_token(),
        );

        let cron_store = Arc::new(baybo_cron::test_support::InMemoryCronStore::new());
        let turn_lifecycle_for_harness = Arc::clone(&turn_lifecycle);
        let actor = AgentActor::from_parts(
            baybo_agent::state::DurableActorState::new(session.clone()),
            baybo_agent::state::VolatileResources {
                agent_loop,
                response_tx: output_tx,
                turn_lifecycle,
                span_recorder,
                actor_token,
                supervisor: None,
                session_manager: Arc::clone(&session_manager),
                cron_store: Arc::clone(&cron_store) as Arc<dyn baybo_store::CronStore>,
                workspace: Arc::clone(&workspace),
            },
        );
        let actor_handle = tokio::spawn(actor.run(mailbox_rx));

        AgentTestHarness {
            session,
            stub_llm,
            gateway,
            vault,
            secret_store,
            turn_store,
            turn_lifecycle: turn_lifecycle_for_harness,
            cost_store,
            trace_store,
            tool_registry,
            skill_registry,
            cost_manager,
            token_calibration,
            session_manager,
            task_store,
            cron_store,
            memory_session_store,
            mailbox: mailbox_tx,
            outputs: output_rx,
            actor_handle: Some(actor_handle),
        }
    }
}

// --- Helpers to obtain `Box<dyn Trait>` handles from `Arc<Concrete>` ---
//
// Each in-memory store is constructed once as an `Arc` and shared
// directly with the managers — `TurnLifecycle` and `CostManager` accept
// `Arc<dyn Trait>`, so the test handle and the manager-owned handle
// point at the same instance and post-run assertions see real state.

fn share_turn_store(arc: &Arc<MemoryTurnStore>) -> Arc<dyn baybo_turn::TurnStore> {
    arc.clone()
}

fn share_cost_store(arc: &Arc<MemoryCostStore>) -> Arc<dyn baybo_cost::CostStore> {
    arc.clone()
}

// Avoid unused-import lints when callers don't reference these types.
#[allow(dead_code)]
fn _typecheck_session(_: &User, _: &ChannelType) {}

/// Wraps the harness's stub so the first non-streaming `chat` parks mid-flight.
///
/// Exists for one question no other fixture can ask: what happens to work
/// already in flight when the turn is cancelled. Streaming calls pass straight
/// through, so only the compaction summariser is held — and only once, so the
/// test can go on to observe the compaction the next turn runs.
struct GatedChat {
    inner: Arc<StubLlm>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    held: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmCompletion for GatedChat {
    async fn chat(
        &self,
        request: &baybo_llm::ChatRequest,
    ) -> baybo_llm::Result<baybo_llm::LlmResponse> {
        if !self.held.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_waiters();
            self.release.notified().await;
        }
        self.inner.chat(request).await
    }

    async fn chat_stream(
        &self,
        request: &baybo_llm::ChatRequest,
    ) -> baybo_llm::Result<baybo_llm::LlmStream> {
        self.inner.chat_stream(request).await
    }

    fn model_info(&self) -> &baybo_llm::ModelInfo {
        self.inner.model_info()
    }

    fn effective_effort(&self, requested: Option<&str>) -> Option<String> {
        self.inner.effective_effort(requested)
    }
}
