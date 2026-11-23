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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use baybo_agent::{
    AgentLoop, SecurityGateway,
    actor::{AgentActor, AgentMessage, mailbox::MailboxSender},
    recovery::TurnRecovery,
    tool_executor::ToolExecutor,
};
use baybo_channels::{AgentEvent, AgentOutput, IncomingMessage, Message};
use baybo_context::{ContextManager, ContextManagerConfig, HeuristicTokenizer};
use baybo_cost::test_support::MemoryCostStore;
use baybo_cost::{CostManager, SpendingLimits, cost_hooks};
use baybo_human_ask::test_support::MemoryHumanAskStore;
use baybo_human_ask::{
    AskCore, AskQueues, ChannelAskGate, HumanAskManager, HumanAskManagerConfig, PolicyDenyGate,
    ResumeSink,
};
use baybo_llm::test_support::StubLlm;
use baybo_llm::{LlmCompletion, ModelPricing};
use baybo_model::{
    ChannelType, ChatMessage, ContentBlock, LlmEntryName, MessageMetadata, Session, User,
};
use baybo_security::test_support::MemorySecretStore;
use baybo_security::{LeakDetector, SecretVault};
use baybo_skills::SkillRegistry;
use baybo_store::{HumanAskRow, HumanAskStore};
use baybo_tools::{ApprovalGate, ChannelGates, QuestionGate, Tool, ToolManifest, ToolRegistry};
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
    pub trace_store: Arc<dyn TraceStore>,
    pub recovery: TurnRecovery,
    pub tool_registry: Arc<ToolRegistry>,
    pub skill_registry: Arc<SkillRegistry>,
    pub cost_manager: Arc<CostManager>,
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
    /// The ask ledger behind the loop's `HumanAskManager` and, with
    /// [`AgentTestHarnessBuilder::with_channel_ask_gate`], the session
    /// channel's gate.
    pub ask_store: Arc<MemoryHumanAskStore>,
    /// The ask ledger actually wired: `ask_store`, or the sqlite one under
    /// [`AgentTestHarnessBuilder::with_store`].
    pub ask_ledger: Arc<dyn HumanAskStore>,
    /// The session store actually wired: `memory_session_store`, or the
    /// sqlite one under [`AgentTestHarnessBuilder::with_store`].
    pub session_store: Arc<dyn baybo_session::SessionStore>,
    /// The ledger + observers the gates and the verbs share, so a test can
    /// install gateway channels over the same asks.
    pub ask_core: Arc<AskCore>,
    /// The ask queues the channel gate parks on.
    pub ask_queues: AskQueues,
    /// The verbs over `ask_store` (resolve, abandon, …).
    pub human_asks: Arc<HumanAskManager>,
    /// Parent of the actor's token. Cancelling it is what process shutdown
    /// does: every turn the actor runs reads the cancel as `Ancestor`.
    pub actor_root_token: tokio_util::sync::CancellationToken,
    /// Supervises the harness session across idle eviction and rehydration.
    pub supervisor: baybo_agent::AgentSupervisor,
    /// LLM requests the strict provider fake refused because a `tool_use`
    /// was not answered by the very next message. Stays 0 unless
    /// [`AgentTestHarnessBuilder::with_strict_tool_pairing`] is on.
    pub strict_rejections: Arc<AtomicUsize>,
    pub mailbox: MailboxSender<AgentMessage>,
    #[cfg(test)]
    actor_spawner: baybo_agent::ActorSpawner,
    outputs: mpsc::Receiver<AgentOutput>,
    router_task: Option<JoinHandle<()>>,
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
        self.send_content(vec![ContentBlock::Text(text.into())])
            .await
    }

    /// [`AgentTestHarness::send_text`] for arbitrary user content — an
    /// uncaptioned image, a caption plus attachments.
    pub async fn send_content(&mut self, content: Vec<ContentBlock>) -> anyhow::Result<()> {
        let message = Message {
            id: format!("msg-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)),
            session_id: self.session.id.clone(),
            channel: self.session.channel.clone(),
            sender: self.session.user.clone(),
            content,
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

    /// Stop routing and join every actor, including actors spawned after eviction.
    pub async fn shutdown(mut self) {
        self.outputs.close();
        self.supervisor.shutdown_all().await;
        if let Some(router) = self.router_task {
            router.await.expect("router exited cleanly");
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
    /// Whether the stub LLM reports `supports_vision`. Defaults to `false`,
    /// the stub's own default.
    model_supports_vision: bool,
    /// Live title surface. Defaults to `None`, which keeps the title pass
    /// off: with one stub serving every call, a title `chat()` would take
    /// the reply a test queued for a compaction or a request count.
    title_sink: Option<Arc<dyn baybo_agent::SessionTitleSink>>,
    /// Install the production `ChannelAskGate` for the session's channel.
    /// Defaults to `false`: the gate map stays empty and every ask is
    /// denied by `AutoDenyGate`, so tests that do not answer asks never park.
    channel_ask_gate: bool,
    /// Transcript rows and ask-ledger rows present before the actor spawns,
    /// as a restart leaves them: the actor hydrates over both.
    seed_transcript: Vec<ChatMessage>,
    seed_asks: Vec<HumanAskRow>,
    /// Persist sessions, transcripts, turns and asks to this sqlite store
    /// instead of the in-memory fakes, so a test can drop the harness and
    /// build another over the same database — a restart.
    store: Option<baybo_storage::Store>,
    /// Run the router to deliver answers to dead asks.
    resume_door: bool,
    /// Refuse any LLM request whose `tool_use` is not answered by the very
    /// next message, as a real provider does.
    strict_tool_pairing: bool,
    background_jobs: Option<Arc<dyn baybo_tools::background::BackgroundJobSink>>,
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
            model_supports_vision: false,
            title_sink: None,
            channel_ask_gate: false,
            seed_transcript: Vec::new(),
            seed_asks: Vec::new(),
            store: None,
            resume_door: false,
            strict_tool_pairing: false,
            background_jobs: None,
        }
    }
}

impl AgentTestHarnessBuilder {
    /// Route the session channel's asks through a real `ChannelAskGate` over
    /// the harness's ledger, so an ask parks until a test resolves it via
    /// [`AgentTestHarness::human_asks`].
    pub fn with_channel_ask_gate(mut self) -> Self {
        self.channel_ask_gate = true;
        self
    }

    /// Back sessions, transcripts, turns, traces and the ask ledger with `store`.
    /// The session row must already be saved in it (asks reference it); the
    /// in-memory `memory_session_store` / `turn_store` / `ask_store` fields
    /// are then unwired — read `session_store` / `ask_ledger` /
    /// `turn_lifecycle` instead.
    pub fn with_store(mut self, store: baybo_storage::Store) -> Self {
        self.store = Some(store);
        self
    }

    /// Route answers to dead asks through the router and its actor spawner.
    pub fn with_resume_door(mut self) -> Self {
        self.resume_door = true;
        self
    }

    /// Make the wired LLM a strict provider: a request with a `tool_use`
    /// not answered by the very next message fails as a bad request and is
    /// counted in [`AgentTestHarness::strict_rejections`].
    pub fn with_strict_tool_pairing(mut self) -> Self {
        self.strict_tool_pairing = true;
        self
    }

    /// Persist `messages` on the session's transcript before the actor
    /// spawns, so its hydration sees them.
    pub fn with_seed_transcript(mut self, messages: Vec<ChatMessage>) -> Self {
        self.seed_transcript = messages;
        self
    }

    /// Put `row` on the ask ledger before the actor spawns.
    pub fn with_seed_ask(mut self, row: HumanAskRow) -> Self {
        self.seed_asks.push(row);
        self
    }

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

    /// Make the stub LLM report `supports_vision`, so paths that gate image
    /// delivery on the model's capability take their vision branch.
    pub fn with_model_vision(mut self, supports_vision: bool) -> Self {
        self.model_supports_vision = supports_vision;
        self
    }

    /// Wire a live title surface, which turns the conversation-title pass
    /// on. Its `chat()` is served by the harness's own stub, so queue a
    /// `push_response` for it.
    pub fn with_title_sink(mut self, sink: Arc<dyn baybo_agent::SessionTitleSink>) -> Self {
        self.title_sink = Some(sink);
        self
    }

    /// Wire the sink background-capable tools hand their work to. Defaults to
    /// `None`, so nothing backgrounds unless a test asks for it.
    pub fn with_background_jobs(
        mut self,
        sink: Arc<dyn baybo_tools::background::BackgroundJobSink>,
    ) -> Self {
        self.background_jobs = Some(sink);
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
    /// only caller in these suites — unless [`Self::with_title_sink`] turns
    /// the title pass on, whose `chat` would take the gate) until `release`
    /// is notified, signalling `entered` first so the test can act while the
    /// call is in flight. Later calls pass straight through, so a test can
    /// hold one compaction and then let the next one run normally.
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

        let trace_event_stream = TraceEventStream::new();
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
            if self.model_supports_vision {
                let mut info = stub.model_info().clone();
                info.supports_vision = true;
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
        let strict_rejections = Arc::new(AtomicUsize::new(0));
        let wired_llm: Arc<dyn baybo_llm::LlmCompletion> = if self.strict_tool_pairing {
            Arc::new(StrictToolPairing {
                inner: wired_llm,
                rejections: Arc::clone(&strict_rejections),
            })
        } else {
            wired_llm
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
        for (tool, manifest) in baybo_human_ask::tools::agent_tools() {
            tool_registry.register(tool, manifest);
        }
        let tool_registry = Arc::new(tool_registry);
        let skill_registry = Arc::new(SkillRegistry::new());
        let gates = ChannelGates::new();
        let ask_store = MemoryHumanAskStore::new();
        let memory_session_store = Arc::new(baybo_session::test_support::MemorySessionStore::new());
        let (session_store, folder_store, turn_store_dyn, trace_store, ask_ledger) =
            match &self.store {
                Some(store) => {
                    assert!(
                        self.seed_transcript.is_empty() && self.seed_asks.is_empty(),
                        "seed a durable harness through its store, not the builder"
                    );
                    (
                        store.session.clone(),
                        store.session_folder.clone(),
                        store.turn.clone(),
                        store.trace.clone(),
                        store.human_ask.clone(),
                    )
                }
                None => {
                    for row in self.seed_asks {
                        ask_store.insert(row);
                    }
                    // Mirror production's "session row exists before the actor
                    // spawns" shape so the transcript-read intercept's
                    // cross-session lookup finds the row instead of NotFound.
                    memory_session_store.seed_session(&session);
                    memory_session_store.seed_messages(&session.id, &self.seed_transcript);
                    (
                        Arc::clone(&memory_session_store) as Arc<dyn baybo_session::SessionStore>,
                        Arc::new(baybo_session::test_support::MemorySessionFolderStore::new())
                            as Arc<dyn baybo_session::SessionFolderStore>,
                        share_turn_store(&turn_store),
                        Arc::new(MemoryTraceStore::new()) as Arc<dyn TraceStore>,
                        Arc::clone(&ask_store) as Arc<dyn HumanAskStore>,
                    )
                }
            };
        let turn_lifecycle = Arc::new(TurnLifecycle::new(turn_store_dyn));
        let session_manager = Arc::new(baybo_agent::SessionManager::new(
            Arc::clone(&session_store),
            folder_store,
        ));
        let ask_core = Arc::new(AskCore::new(Arc::clone(&ask_ledger)));
        let ask_queues = AskQueues::new();
        let (resume_sink, resume_rx) = baybo_agent::runtime::asks::RouterResumeSink::channel();
        let resume_sink: Arc<dyn ResumeSink> = if self.resume_door {
            Arc::new(resume_sink)
        } else {
            Arc::new(baybo_human_ask::test_support::NoResume)
        };
        let human_asks = Arc::new(HumanAskManager::from_config(HumanAskManagerConfig {
            core: Arc::clone(&ask_core),
            queues: ask_queues.clone(),
            resume_sink,
        }));
        if self.channel_ask_gate {
            let gate = Arc::new(ChannelAskGate::new(
                ask_queues.get_or_create(&session.channel),
                Arc::clone(&ask_core),
            ));
            gates.approvals.insert(
                session.channel.clone(),
                Arc::clone(&gate) as Arc<dyn ApprovalGate>,
            );
            if session.channel == ChannelType::owner() {
                gates
                    .questions
                    .insert(session.channel.clone(), gate as Arc<dyn QuestionGate>);
            }
        }
        let virtual_reads: Option<Arc<dyn baybo_tools::VirtualReadResolver>> =
            Some(Arc::new(baybo_agent::SessionTranscriptReader::new(
                Arc::clone(&session_manager) as Arc<dyn baybo_agent::SessionTranscript>,
                baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from("/tmp")),
            )));
        let tool_executor = Arc::new(ToolExecutor::new(
            tool_registry.clone(),
            gates,
            gateway.clone(),
            std::path::PathBuf::from("/tmp"),
            baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from("/tmp")),
            None,
            None,
            virtual_reads,
            self.background_jobs,
            None,
            None,
            Arc::new(PolicyDenyGate::new(Arc::clone(&ask_core))),
        ));

        let tokenizer = Arc::new(HeuristicTokenizer);
        // Non-existent root: nothing under it is read in these tests, so
        // there is no tempdir to clean up.
        let workspace = self.workspace.unwrap_or_else(|| {
            Arc::new(baybo_workspace::WorkspacePaths::new(PathBuf::from(
                "/nonexistent-baybo-it-workspace",
            )))
        });
        let keep_recent = self.keep_recent.unwrap_or(50);
        let compression_threshold = self.compression_threshold.unwrap_or(0.95);
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

        let actor_root_token = tokio_util::sync::CancellationToken::new();

        let (output_tx, output_rx) = mpsc::channel(self.output_capacity);

        // Mirror production's turn-state projector so the turn-*ended*
        // `TurnState { active: false }` is derived from the turn store
        // (the actor only emits the leading `true`). Without it the e2e
        // outputs would never carry the close edge.
        baybo_agent::supervisor::spawn_turn_state_projector(
            Arc::clone(&turn_lifecycle),
            Arc::clone(&session_manager),
            output_tx.clone(),
            actor_root_token.child_token(),
        );

        let cron_store = Arc::new(baybo_cron::test_support::InMemoryCronStore::new());
        let cron_ledger = baybo_cron::CronDeliveryLedger::new(cron_store.clone());

        let recovery = TurnRecovery::new(Arc::clone(&trace_store), Arc::clone(&turn_lifecycle));
        let supervisor = baybo_agent::AgentSupervisor::new(
            output_tx.clone(),
            Arc::clone(&human_asks) as Arc<dyn baybo_human_ask::LivePendingAsks>,
            actor_root_token.clone(),
            recovery.clone(),
        );

        let actor_spawner: baybo_agent::router::ActorSpawner = {
            let trace_store = trace_store.clone();
            let skill_registry = skill_registry.clone();
            let session_manager = session_manager.clone();
            let workspace = workspace.clone();
            let tool_registry = tool_registry.clone();
            let gateway = gateway.clone();
            let task_store = task_store.clone();
            let human_asks = human_asks.clone();
            let turn_lifecycle = turn_lifecycle.clone();
            let supervisor = supervisor.clone();
            let cron_ledger = cron_ledger.clone();
            Arc::new(move |session, pins, registry, response_tx, actor_token| {
                let context_manager = ContextManager::from_config(ContextManagerConfig {
                    agent: None,
                    tokenizer: tokenizer.clone(),
                    workspace: Arc::clone(&workspace),
                    keep_recent,
                    compression_threshold,
                    // The harness drives compaction through the window share and a
                    // deliberately small stub window; an absolute cap on top would
                    // be a second trigger every such test has to reason about.
                    max_active_tokens: 0,
                    skill_registry: Arc::clone(&skill_registry),
                    channel: session.channel.clone(),
                    shape: baybo_context::prompts::soul::PromptShape::for_trigger(&session.trigger),
                    session_id: session.id.clone(),
                    sessions: Arc::clone(&session_manager),
                    subagent_profile: Some((subagent_registry.clone(), "harness".to_string())),
                    builtin_memory: false,
                    deferred_tool_servers: Vec::new(),
                });
                let agent_loop = AgentLoop::from_config(baybo_agent::agent_loop::AgentLoopConfig {
                    llm_pool: llm_pool.clone(),
                    initial_llm: pins.entry,
                    initial_model: pins.model,
                    initial_effort: pins.effort,
                    tool_registry: tool_registry.clone(),
                    tool_executor: tool_executor.clone(),
                    context_manager,
                    max_iterations: 20,
                    security_gateway: gateway.clone(),
                    // Mirror production for the progress observer's durable shadow and
                    // title-generation paths.
                    sessions: Some(Arc::clone(&session_manager)),
                    memory: self.memory.clone(),
                    task_store: task_store.clone(),
                    title_sink: self.title_sink.clone(),
                    asks: human_asks.clone(),
                });
                let span_recorder = Arc::new(SpanRecorder::new(
                    session.id.clone(),
                    session.user.id.clone(),
                    Arc::clone(&trace_store),
                    trace_event_stream.clone(),
                ));
                let actor = AgentActor::from_parts(
                    baybo_agent::state::DurableActorState::new(session),
                    baybo_agent::state::VolatileResources {
                        agent_loop,
                        response_tx,
                        turn_lifecycle: turn_lifecycle.clone(),
                        span_recorder,
                        actor_token,
                        supervisor: registry,
                        session_manager: Arc::clone(&session_manager),
                        cron_ledger: cron_ledger.clone(),
                        workspace: Arc::clone(&workspace),
                    },
                );
                supervisor.spawn_actor(actor, self.mailbox_capacity)
            })
        };
        let mailbox_tx = actor_spawner(
            session.clone(),
            baybo_model::LlmPin::unpinned(),
            Some(supervisor.clone()),
            output_tx,
            actor_root_token.child_token(),
        );
        assert!(
            supervisor
                .register_if_absent(session.id.clone(), mailbox_tx.clone())
                .is_ok()
        );
        let router_task = self.resume_door.then(|| {
            let agent_profiles = self
                .store
                .as_ref()
                .map(|store| store.agent_profile.clone())
                .unwrap_or_else(|| {
                    Arc::new(baybo_store::test_support::MemoryAgentProfileStore::new())
                });
            let router = baybo_agent::Router::from_config(baybo_agent::router::RouterConfig {
                session_manager: session_manager.clone(),
                supervisor: supervisor.clone(),
                channels: Arc::new(baybo_channels::ChannelRegistry::new()),
                security_gateway: gateway.clone(),
                cost_manager: cost_manager.clone(),
                actor_spawner: actor_spawner.clone(),
                turn_lifecycle: turn_lifecycle.clone(),
                cron_ledger,
                agent_profiles,
                cron_trigger_rx: mpsc::channel(1).1,
                issue_run_rx: None,
                ask_resume_rx: Some(resume_rx),
                board: None,
                rate_limit: baybo_agent::router::LiveRateLimit::new(1, Duration::from_secs(1)),
                workspace,
                inbound_dedup: Arc::new(baybo_channels::InboundDedup::new()),
            });
            tokio::spawn(router.run(mpsc::channel(1).1, mpsc::channel(1).1))
        });

        AgentTestHarness {
            session,
            stub_llm,
            gateway,
            vault,
            secret_store,
            turn_store,
            turn_lifecycle,
            cost_store,
            trace_store,
            recovery,
            tool_registry,
            skill_registry,
            cost_manager,
            session_manager,
            task_store,
            cron_store,
            memory_session_store,
            ask_store,
            ask_ledger,
            session_store,
            ask_core,
            ask_queues,
            human_asks,
            actor_root_token,
            supervisor,
            strict_rejections,
            #[cfg(test)]
            actor_spawner,
            mailbox: mailbox_tx,
            outputs: output_rx,
            router_task,
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

    fn delivers_image_block(&self, block: &baybo_model::ContentBlock) -> bool {
        self.inner.delivers_image_block(block)
    }

    fn audio_delivery(&self) -> baybo_llm::AudioDelivery {
        self.inner.audio_delivery()
    }
}

/// A provider that rejects a transcript whose `tool_use` is not answered
/// right after it — what Anthropic's API does, and what a misplaced
/// settlement result would trip in production. "Right after" is the run of
/// tool rows that follows, which the request builder merges into the one
/// message the API sees.
struct StrictToolPairing {
    inner: Arc<dyn LlmCompletion>,
    rejections: Arc<AtomicUsize>,
}

impl StrictToolPairing {
    fn check(&self, request: &baybo_llm::ChatRequest) -> baybo_llm::Result<()> {
        let messages = &request.messages;
        for (i, message) in messages.iter().enumerate() {
            for block in &message.content {
                let ContentBlock::ToolUse { id, .. } = block else {
                    continue;
                };
                let answered = messages[i + 1..]
                    .iter()
                    .take_while(|next| next.role == baybo_model::Role::Tool)
                    .flat_map(|next| &next.content)
                    .any(|b| {
                        matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
                    });
                if !answered {
                    self.rejections.fetch_add(1, Ordering::SeqCst);
                    return Err(baybo_llm::LlmError::BadRequest(format!(
                        "tool_use {id} is not followed by its tool_result"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl LlmCompletion for StrictToolPairing {
    async fn chat(
        &self,
        request: &baybo_llm::ChatRequest,
    ) -> baybo_llm::Result<baybo_llm::LlmResponse> {
        self.check(request)?;
        self.inner.chat(request).await
    }

    async fn chat_stream(
        &self,
        request: &baybo_llm::ChatRequest,
    ) -> baybo_llm::Result<baybo_llm::LlmStream> {
        self.check(request)?;
        self.inner.chat_stream(request).await
    }

    fn model_info(&self) -> &baybo_llm::ModelInfo {
        self.inner.model_info()
    }

    fn effective_effort(&self, requested: Option<&str>) -> Option<String> {
        self.inner.effective_effort(requested)
    }

    fn delivers_image_block(&self, block: &baybo_model::ContentBlock) -> bool {
        self.inner.delivers_image_block(block)
    }

    fn audio_delivery(&self) -> baybo_llm::AudioDelivery {
        self.inner.audio_delivery()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_agent::mailbox::TrySendError;

    #[tokio::test]
    async fn actor_panic_closes_the_recorded_turn_step_and_span_in_sqlite() {
        use baybo_llm::{ChatRequest, LlmResponse, LlmStream, ModelInfo};
        use baybo_trace::{LifecycleOutcome, LifecycleState, Span, SpanKind, Step};
        use baybo_turn::{CancelReason, TurnStatus};

        struct PanickingLlm(StubLlm);

        #[async_trait::async_trait]
        impl LlmCompletion for PanickingLlm {
            async fn chat(&self, request: &ChatRequest) -> baybo_llm::Result<LlmResponse> {
                self.0.chat(request).await
            }

            async fn chat_stream(&self, _: &ChatRequest) -> baybo_llm::Result<LlmStream> {
                panic!("simulated actor panic during an LLM call")
            }

            fn model_info(&self) -> &ModelInfo {
                self.0.model_info()
            }

            fn effective_effort(&self, requested: Option<&str>) -> Option<String> {
                self.0.effective_effort(requested)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("baybo.db");
        let store = baybo_storage::Store::open(&db).await.unwrap();
        let session = SessionBuilder::new().build();
        store.session.save(&session).await.unwrap();
        let mut harness = AgentTestHarness::builder()
            .session(session.clone())
            .with_store(store)
            .with_llm(Arc::new(PanickingLlm(StubLlm::new())))
            .build();
        harness.send_text("hello").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(output) = harness.outputs.recv().await {
                if matches!(
                    output.event,
                    AgentEvent::Notice {
                        level: baybo_channels::NoticeLevel::Error,
                        ..
                    }
                ) {
                    return;
                }
            }
            panic!("actor exited before sending the crash notice")
        })
        .await
        .expect("actor panic recovery must complete");
        harness.shutdown().await;

        let store = baybo_storage::Store::open(&db).await.unwrap();
        let lifecycle = TurnLifecycle::new(store.turn);
        let turns = lifecycle.list_by_session(&session.id, None).await.unwrap();
        let [turn] = turns.as_slice() else {
            panic!("expected one turn: {turns:?}")
        };
        assert!(matches!(
            turn.status,
            TurnStatus::Cancelled {
                reason: CancelReason::SystemCrash,
                ..
            }
        ));
        let crashed = LifecycleState::Done(LifecycleOutcome::Cancelled {
            reason: CancelReason::SystemCrash,
        });
        let steps = store.trace.list_steps_by_turn(&turn.id).await.unwrap();
        let step = steps
            .into_iter()
            .map(|row| Step::from_row(row).unwrap())
            .find(|step| step.outcome == crashed)
            .expect("crashed step");
        let spans = store.trace.list_spans_by_step(&step.id).await.unwrap();
        let span = spans
            .into_iter()
            .map(|row| Span::from_row(row).unwrap())
            .find(|span| matches!(span.kind, SpanKind::LlmCall { .. }))
            .expect("LLM span");
        assert_eq!(span.outcome, crashed);
    }

    fn spawn_oneshot(
        harness: &AgentTestHarness,
    ) -> (MailboxSender<AgentMessage>, mpsc::Receiver<AgentOutput>) {
        let (tx, rx) = mpsc::channel(1);
        let mailbox = (harness.actor_spawner)(
            harness.session.clone(),
            baybo_model::LlmPin::unpinned(),
            None,
            tx,
            harness.supervisor.root_token().child_token(),
        );
        (mailbox, rx)
    }

    #[tokio::test(start_paused = true)]
    async fn one_shot_exit_preserves_the_chat_mailbox_for_the_same_session() {
        let harness = AgentTestHarness::builder().build();
        let (once, mut outputs) = spawn_oneshot(&harness);
        once.send(AgentMessage::ActorStop).await.unwrap();
        // The panic observer holds the last output sender through exit cleanup.
        assert!(
            tokio::time::timeout(Duration::from_secs(5), outputs.recv())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            harness
                .supervisor
                .route(&harness.session.id, AgentMessage::ActorStop)
                .await
        );
        harness.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_joins_open_mailboxes_and_rejects_a_racing_spawn() {
        let harness = AgentTestHarness::builder().build();
        let (once, _outputs) = spawn_oneshot(&harness);
        let shutdown = harness.supervisor.root_token();
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(harness.supervisor.shutdown_all(), async {
                shutdown.cancelled().await;
                let (late, _) = spawn_oneshot(&harness);
                assert!(matches!(
                    late.try_send(AgentMessage::ActorStop),
                    Err(TrySendError::Closed(_))
                ));
            });
        })
        .await
        .expect("idle actors exit while their mailbox senders are still alive");
        for mailbox in [&once, &harness.mailbox] {
            assert!(matches!(
                mailbox.try_send(AgentMessage::ActorStop),
                Err(TrySendError::Closed(_))
            ));
        }
        assert!(harness.supervisor.is_empty());
        harness.shutdown().await;
    }
}
