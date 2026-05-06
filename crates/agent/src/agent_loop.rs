use std::sync::Arc;

use aura_channels::{AgentOutput, OutgoingMessage};
use aura_context::ContextManager;
use aura_job::{JobInput, JobOutput};
use aura_llm::{
    ChatRequest, LlmCompletion, LlmResponse, StreamEvent, TokenUsage, ToolDefinitionForLlm,
};
use aura_model::{ChatMessage, ContentBlock, JobId, Role};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::memory::MemoryManager;
use aura_model::Session;
use aura_skills::{SkillRegistry, SkillSummary};
use aura_skills_assessor::SkillAssessor;
use aura_tools::{ToolOutput, ToolRegistry};
use aura_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanFinalize, SpanKind, StepHandle, StepKind,
};
use tracing::{debug, info, warn};

use crate::error_recovery::ErrorHandler;
use crate::job::{JobLifecycle, JobSpec};
use crate::policy::ExecutionPolicy;
use crate::security::SecurityGateway;
use crate::session_log::{
    LlmCallOutcome, LlmCallRecord, LlmRequestMeta, LlmResponseMeta, SessionLlmLogger,
};
use crate::soul::Soul;
use crate::subagent::{
    SPAWN_SUBAGENT_TOOL_NAME, SubagentExitStatus, SubagentResult, SubagentRuntime,
    parse_spawn_request,
};
use crate::tool_executor::ToolExecutor;
use crate::trace::SpanRecorder;
use tokio_util::sync::CancellationToken;

/// The maximum amount of text we'll hold in the streaming buffer waiting
/// for a placeholder to complete. If a chunk ends with an open `[{` but no
/// closing `}]` arrives within this many bytes, we flush anyway — no real
/// placeholder is this long, so holding further would be a DoS vector.
const STREAM_BUFFER_HIGH_WATER: usize = 128;

/// Cap on attachments carried into the final `OutgoingMessage`. Tools
/// like `browser_screenshot` and `send_local_file` push into a
/// per-turn `accumulated_attachments` vec; without a cap a chatty
/// agent (multi-iteration browse, page-each screenshot, …) would
/// dump every blob produced over the loop into one channel message.
/// 16 is well above any plausible "user asked for N artifacts" case
/// and small enough that a runaway loop doesn't drown the channel.
/// FIFO eviction: keep the newest, drop the oldest — older
/// screenshots from earlier iterations are typically stale anyway
/// (page state has moved on).
const MAX_ATTACHMENTS_PER_TURN: usize = 16;

/// Compute the byte index at which it is safe to flush `pending` to the
/// output stream. Anything after that index is withheld because it might
/// be the beginning of a `[{REDACTED_SECRET_...}]` placeholder split
/// across chunks.
///
/// Returns `pending.len()` when no partial placeholder is pending, or a
/// smaller value pointing at the earliest `[` of a potential placeholder.
/// If the buffer grows past `STREAM_BUFFER_HIGH_WATER` we flush the whole
/// thing to avoid unbounded buffering on pathological input.
fn safe_flush_boundary(pending: &str) -> usize {
    if pending.len() > STREAM_BUFFER_HIGH_WATER {
        return pending.len();
    }
    // Placeholders open with `[{`. If the last `[{` has no matching `}]`
    // after it, it might be a placeholder split across chunks — withhold
    // from that `[{`. A lone trailing `[` could become `[{` when the next
    // chunk lands, so withhold it too.
    if let Some(idx) = pending.rfind("[{") {
        let tail = &pending[idx..];
        if !tail.contains("}]") {
            return idx;
        }
    }
    if pending.ends_with('[') {
        return pending.len() - 1;
    }
    pending.len()
}

/// Append `items` into `dst` while keeping `dst.len() <=
/// MAX_ATTACHMENTS_PER_TURN` via FIFO eviction. Used for the
/// per-turn `accumulated_attachments` vec so a runaway loop can't
/// drown the final `OutgoingMessage` in stale screenshots.
fn push_bounded<I: IntoIterator<Item = ContentBlock>>(dst: &mut Vec<ContentBlock>, items: I) {
    for item in items {
        if dst.len() >= MAX_ATTACHMENTS_PER_TURN {
            // Stable order: channels render attachments in arrival
            // order, so FIFO eviction keeps the surviving set
            // chronologically coherent.
            dst.remove(0);
        }
        dst.push(item);
    }
}

/// `try_send` drops on a full channel: notices are non-load-bearing
/// (the verdict still lands in the trace) and `SessionNotifier::emit`
/// is sync, so blocking the tool path on backpressure would be worse
/// than losing the line.
struct DeltaTxNotifier {
    tx: tokio::sync::mpsc::Sender<AgentOutput>,
    session_id: String,
    user_id: String,
    channel: aura_model::ChannelType,
}

impl aura_tools::SessionNotifier for DeltaTxNotifier {
    fn emit(&self, level: aura_tools::NoticeLevel, summary: &str, detail: &str) {
        let level = match level {
            aura_tools::NoticeLevel::Info => aura_channels::NoticeLevel::Info,
            aura_tools::NoticeLevel::Warn => aura_channels::NoticeLevel::Warn,
            aura_tools::NoticeLevel::Error => aura_channels::NoticeLevel::Error,
        };
        let text = if detail.is_empty() {
            summary.to_string()
        } else {
            format!("{summary}: {detail}")
        };
        let _ = self.tx.try_send(AgentOutput::Notice {
            session_id: self.session_id.clone(),
            user_id: self.user_id.clone(),
            channel: self.channel.clone(),
            level,
            text,
        });
    }
}

/// What one `LlmIteration` step's body produced. The terminal-vs-loop
/// distinction lives here (rather than the body short-circuiting via
/// `?`) so the `with_step` wrapper sees a clean `Ok(...)` either way
/// and closes the step before the parent loop runs the next thing.
enum IterationOutcome {
    /// Final assistant response — caller returns this from `run_inner`.
    Final(OutgoingMessage),
    /// LLM emitted tool calls; loop continues. `deferred_subagents`
    /// must be dispatched as **peer** steps (each its own
    /// `StepKind::Subagent`) after the iteration step closes — per
    /// `trace.md`, steps cannot nest.
    Continue {
        deferred_subagents: Vec<(String, serde_json::Value)>,
    },
}

/// Core conversation loop: LLM call -> parse -> Tool/Skill dispatch -> repeat.
pub struct AgentLoop {
    llm_client: Arc<dyn LlmCompletion>,
    tool_registry: Arc<ToolRegistry>,
    skill_registry: Arc<SkillRegistry>,
    tool_executor: Arc<ToolExecutor>,
    context_manager: ContextManager,
    memory_manager: Arc<MemoryManager>,
    policy: ExecutionPolicy,
    soul: Soul,
    security_gateway: Arc<SecurityGateway>,
    error_handler: ErrorHandler,
    /// Optional subagent runtime. When set, LLM `tool_use` calls
    /// targeting `spawn_subagent` short-circuit the regular
    /// tool_executor path and route through here. Unset →
    /// `spawn_subagent` calls return a synthetic
    /// `SubagentExitStatus::Failed("no subagent runtime registered")`
    /// tool result.
    subagent_runtime: Option<Arc<dyn SubagentRuntime>>,
    /// Optional LLM risk assessor. When set, every skill candidate is
    /// checked before injection: `Dangerous` verdicts veto the skill,
    /// `Suspicious` verdicts log a warning but allow it through.
    skill_assessor: Option<Arc<SkillAssessor>>,
    /// Optional per-session JSONL logger for LLM calls. When set, every
    /// `call_llm` invocation appends a record (request, response or
    /// error, latency, model metadata) to
    /// `<workspace>/logs/sessions/<session_id>.jsonl`.
    session_log: Option<Arc<SessionLlmLogger>>,
    /// Optional cost gate + ledger. When set, [`Self::run_iteration`]
    /// rejects via [`crate::cost::CostManager::check`] *before*
    /// dispatching the next LLM call, and [`Self::call_llm`] feeds the
    /// observed token counts (success or partial-on-error) back via
    /// [`crate::cost::CostManager::record_call`] so the next iteration's
    /// gate sees the spend immediately.
    cost_manager: Option<Arc<crate::cost::CostManager>>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        llm_client: Arc<dyn LlmCompletion>,
        tool_registry: Arc<ToolRegistry>,
        skill_registry: Arc<SkillRegistry>,
        tool_executor: Arc<ToolExecutor>,
        context_manager: ContextManager,
        memory_manager: Arc<MemoryManager>,
        policy: ExecutionPolicy,
        soul: Soul,
        security_gateway: Arc<SecurityGateway>,
    ) -> Self {
        Self {
            llm_client,
            tool_registry,
            skill_registry,
            tool_executor,
            context_manager,
            memory_manager,
            policy,
            soul,
            security_gateway,
            error_handler: ErrorHandler::default(),
            subagent_runtime: None,
            skill_assessor: None,
            session_log: None,
            cost_manager: None,
        }
    }

    /// Attach the subagent runtime so LLM-emitted `spawn_subagent`
    /// tool calls route through it instead of the regular tool
    /// catalogue.
    pub fn with_subagent_runtime(mut self, rt: Arc<dyn SubagentRuntime>) -> Self {
        self.subagent_runtime = Some(rt);
        self
    }

    pub fn with_skill_assessor(mut self, assessor: Arc<SkillAssessor>) -> Self {
        self.skill_assessor = Some(assessor);
        self
    }

    pub fn with_session_log(mut self, logger: Arc<SessionLlmLogger>) -> Self {
        self.session_log = Some(logger);
        self
    }

    pub fn with_cost_manager(mut self, manager: Arc<crate::cost::CostManager>) -> Self {
        self.cost_manager = Some(manager);
        self
    }

    /// Run the main conversation loop for a single user message.
    ///
    /// When `delta_tx` is `Some`, each text chunk emitted by the LLM is
    /// forwarded as `AgentOutput::Delta` so adapters that support partial
    /// rendering (e.g. the TUI) can show incremental output. The final
    /// `OutgoingMessage` returned here should still be dispatched by the
    /// caller as `AgentOutput::Message` so non-streaming adapters receive
    /// the canonical response.
    // Each parameter is genuinely independent (provenance, LLM input,
    // facade handles, lineage hint, streaming sink). Grouping them
    // into a struct would obscure the call site without saving
    // anything.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &mut self,
        session: &mut Session,
        job_input: JobInput,
        user_content: Vec<ContentBlock>,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        parent_job_id: Option<JobId>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<OutgoingMessage> {
        // `job_input` records why this job exists (provenance: which
        // trigger kicked it off — User / Cron / System / Spawned).
        // `user_content` is what we feed the LLM as the first user
        // message. They coincide for `UserChat` but differ for
        // `Cron` / `System` where the input is a synthesized prompt
        // rather than the raw trigger payload.
        let spec = JobSpec {
            session_id: session.id.clone(),
            session_trigger_kind: session.trigger.kind(),
            input: job_input,
            effective_soul_version: session.bound_soul_version.clone(),
            parent_job_id,
        };
        crate::scope::with_job(
            job_lifecycle,
            cancel_token.clone(),
            spec,
            |job_id| async move {
                let outgoing = self
                    .run_inner(
                        session,
                        user_content,
                        job_lifecycle,
                        span_recorder,
                        job_id,
                        delta_tx,
                        cancel_token,
                    )
                    .await?;
                let output = JobOutput::Message {
                    content: outgoing.content.clone(),
                };
                Ok((output, outgoing))
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
        &mut self,
        session: &mut Session,
        user_content: Vec<ContentBlock>,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<OutgoingMessage> {
        let _ = job_lifecycle;
        self.ensure_system_prompt(session).await;

        // Bound to the *outer* delta_tx, not iter_delta_tx — notices
        // need to reach the channel on iter-2+ where streaming is
        // suppressed.
        let notifier: Option<Arc<dyn aura_tools::SessionNotifier>> = delta_tx.as_ref().map(|tx| {
            Arc::new(DeltaTxNotifier {
                tx: tx.clone(),
                session_id: session.id.to_string(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
            }) as Arc<dyn aura_tools::SessionNotifier>
        });

        // Recall relevant memories
        let memories = self
            .memory_manager
            .recall(&session.user.id, &user_content)
            .await?;
        if !memories.is_empty() {
            debug!(count = memories.len(), "recalled memories");
            let memory_text: String = memories
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let memory_msg = ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(format!(
                    "[Memory Context]\n{memory_text}"
                ))],
            };
            self.append_context_message(session, &memory_msg).await?;
        }

        // Append user message (auto-compresses if over token budget)
        let user_msg = ChatMessage {
            role: Role::User,
            content: user_content.clone(),
        };
        self.append_context_message(session, &user_msg).await?;

        let user_text = aura_llm::multimodal::extract_text(&user_content);
        let skills_for_turn = if self.skill_registry.is_empty() {
            Vec::new()
        } else {
            self.invocable_skills()
        };

        if let Some(reminder) = build_skill_reminder(&skills_for_turn) {
            let reminder_msg = ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(reminder)],
            };
            self.append_context_message(session, &reminder_msg).await?;
        }

        session.state.active_skills = skills_for_turn.iter().map(|s| s.name.clone()).collect();

        if let Some((skill_name, args)) = detect_slash_invocation(&user_text, &skills_for_turn) {
            let synthesized_id = format!("synthskill-{}", uuid::Uuid::new_v4());
            let mut input = serde_json::json!({ "skill": skill_name });
            if !args.is_empty() {
                input["args"] = serde_json::Value::String(args);
            }
            let tool_use_block = ContentBlock::ToolUse {
                id: synthesized_id.clone(),
                name: "Skill".to_string(),
                input: input.clone(),
                signature: None,
            };
            let assistant_msg = ChatMessage {
                role: Role::Assistant,
                content: vec![tool_use_block],
            };
            self.append_context_message(session, &assistant_msg).await?;

            let approved = std::sync::Arc::new(parking_lot::Mutex::new(
                session.state.approved_resources.clone(),
            ));

            let executor_clone = Arc::clone(&self.tool_executor);
            let span_recorder_clone = Arc::clone(span_recorder);
            let session_id_clone = session.id.clone();
            let user_clone = session.user.clone();
            let approved_clone = Arc::clone(&approved);
            let synth_id_clone = synthesized_id.clone();
            let input_clone = input.clone();
            let cancel_token_clone = cancel_token.clone();
            let notifier_clone = notifier.clone();

            let result_text = crate::scope::with_step(
                span_recorder.as_ref(),
                job_id,
                StepKind::LlmIteration,
                Some((&cancel_token, aura_job::CancelReason::ParentCancelled)),
                move |step| async move {
                    let res = executor_clone
                        .execute(
                            "Skill",
                            input_clone,
                            &session_id_clone,
                            &user_clone,
                            &approved_clone,
                            &span_recorder_clone,
                            &step,
                            None,
                            synth_id_clone,
                            None,
                            Some(job_id),
                            cancel_token_clone.child_token(),
                            notifier_clone,
                        )
                        .await;
                    let text = match res {
                        Ok(ToolOutput::Text(s)) => s,
                        Ok(ToolOutput::Json(v)) => v.to_string(),
                        Ok(ToolOutput::WithAttachments { text, .. })
                        | Ok(ToolOutput::MultiModalText { text, .. }) => text,
                        Ok(ToolOutput::Error(msg)) => format!("Error: {msg}"),
                        Err(e) => format!("Error: {e}"),
                    };
                    Ok((LifecycleOutcome::Ok, text))
                },
            )
            .await?;

            session.state.approved_resources = approved.lock().clone();

            let tool_msg = ChatMessage {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: synthesized_id,
                    content: result_text,
                }],
            };
            self.append_context_message(session, &tool_msg).await?;
        }

        // Iterative LLM loop
        let mut iterations = 0;
        // Tool invocations issued during intermediate iterations. Channels
        // only receive the terminal `OutgoingMessage`, so without this the
        // TUI (which renders `ContentBlock::ToolUse`) would never see tool
        // activity — breaking any channel-side behavior keyed on the tool
        // call (e.g. the TUI cron-recurring hint).
        let mut accumulated_tool_uses: Vec<ContentBlock> = Vec::new();
        // Side-channel attachments emitted by tools (e.g. send_local_file).
        // Hoisted into the final OutgoingMessage so the channel sidecar
        // delivers them out-of-band; never echoed back to the LLM.
        let mut accumulated_attachments: Vec<ContentBlock> = Vec::new();
        loop {
            // Cooperative cancel checkpoint between iterations. Without
            // this, a `cancel(job_id, ...)` admin call (which trips the
            // registered token before flipping the row) lets the loop
            // finish whatever it's doing and run another LLM call /
            // compress / tool-call before observing the cancel. Tools
            // and the LLM still get the token via their own paths;
            // this catches the orchestration-layer wait windows.
            if cancel_token.is_cancelled() {
                warn!(job_id = %job_id, iterations, "cancel observed at iteration boundary; aborting loop");
                return Err(anyhow::anyhow!("agent loop cancelled"));
            }
            if iterations >= self.policy.max_iterations {
                warn!(max = self.policy.max_iterations, "max iterations reached");
                break;
            }
            iterations += 1;

            // Proactive compression before building the ChatRequest.
            self.compress_if_needed(session, span_recorder, job_id, &cancel_token)
                .await?;

            // Deltas are only streamed on the first iteration of the loop.
            let iter_delta_tx = if iterations == 1 {
                delta_tx.as_ref()
            } else {
                None
            };

            let outcome = crate::scope::with_step(
                span_recorder.as_ref(),
                job_id,
                StepKind::LlmIteration,
                Some((&cancel_token, aura_job::CancelReason::ParentCancelled)),
                |step| {
                    let fut = self.run_iteration(
                        session,
                        span_recorder,
                        step,
                        job_id,
                        iterations,
                        iter_delta_tx,
                        notifier.clone(),
                        &cancel_token,
                        &mut accumulated_tool_uses,
                        &mut accumulated_attachments,
                    );
                    async move { Ok((LifecycleOutcome::Ok, fut.await?)) }
                },
            )
            .await?;

            match outcome {
                IterationOutcome::Final(msg) => return Ok(msg),
                IterationOutcome::Continue { deferred_subagents } => {
                    // Now that the iteration step is closed, dispatch
                    // any deferred subagent calls as peer steps. Each
                    // invocation appends a tool_result message into
                    // context so the next iteration's LLM call sees the
                    // outcome.
                    for (tool_use_id, arguments) in deferred_subagents {
                        let raw = match self
                            .dispatch_subagent(
                                session,
                                job_id,
                                span_recorder,
                                arguments,
                                cancel_token.clone(),
                            )
                            .await
                        {
                            Ok(text) => text,
                            Err(e) => format!("[spawn_subagent error: {e}]"),
                        };
                        let wrapped = self
                            .security_gateway
                            .wrap_tool_output_for_llm(SPAWN_SUBAGENT_TOOL_NAME, &raw);
                        let tool_msg = ChatMessage {
                            role: Role::Tool,
                            content: vec![ContentBlock::ToolResult {
                                tool_use_id,
                                content: wrapped,
                            }],
                        };
                        self.append_context_message(session, &tool_msg).await?;
                    }
                }
            }
        }

        // If we exhausted iterations, return what we have. Tail-append any
        // attachments the tools produced so the user still receives the
        // file even when the agent ran out of reasoning budget.
        let mut content = vec![ContentBlock::Text(
            "I've reached the maximum number of processing steps. Please try again with a simpler request.".to_string(),
        )];
        content.extend(std::mem::take(&mut accumulated_attachments));
        Ok(OutgoingMessage {
            session_id: session.id.to_string(),
            user_id: session.user.id.clone(),
            channel: session.channel.clone(),
            content,
            reply_to: None,
            metadata: Default::default(),
        })
    }

    /// One iteration of the agentic loop, scoped to a single
    /// `LlmIteration` step (opened by [`crate::scope::with_step`] in
    /// the caller). Calls the LLM, executes any non-subagent tool
    /// calls, appends their results to context. `spawn_subagent` calls
    /// are deferred — returned in [`IterationOutcome::Continue`] so
    /// the caller can dispatch them as **peer** steps once `with_step`
    /// has closed this iteration's step (steps cannot nest per
    /// `trace.md`).
    ///
    /// Returns [`IterationOutcome::Final`] when the LLM produced no
    /// tool calls — that's the final assistant response and the loop
    /// terminates.
    #[allow(clippy::too_many_arguments)]
    async fn run_iteration(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        step: StepHandle,
        job_id: JobId,
        iterations: usize,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        notifier: Option<Arc<dyn aura_tools::SessionNotifier>>,
        cancel_token: &CancellationToken,
        accumulated_tool_uses: &mut Vec<ContentBlock>,
        accumulated_attachments: &mut Vec<ContentBlock>,
    ) -> anyhow::Result<IterationOutcome> {
        let (response, llm_span_id) = self
            .call_llm_with_retry(session, span_recorder, &step, delta_tx)
            .await?;

        // If no tool calls, we have the final response.
        if response.tool_calls.is_empty() {
            // Use content_blocks when available, falling back to the
            // text string.
            let response_blocks = if response.content_blocks.is_empty() {
                vec![ContentBlock::Text(response.content.clone())]
            } else {
                response.content_blocks.clone()
            };

            // Append the tool_use blocks issued during intermediate
            // iterations after the final narration so channels that key
            // off them (e.g. the TUI cron hint) can render below the
            // assistant's reply.
            let mut final_blocks = response_blocks.clone();
            final_blocks.extend(std::mem::take(accumulated_tool_uses));
            final_blocks.extend(std::mem::take(accumulated_attachments));

            let final_text = aura_llm::multimodal::extract_text(&response_blocks);

            info!(
                iterations,
                content_len = final_text.len(),
                "conversation loop complete"
            );

            // Append only the final response blocks to context —
            // intermediate tool_use blocks were already appended in
            // prior iterations.
            let assistant_msg = ChatMessage {
                role: Role::Assistant,
                content: response_blocks,
            };
            self.append_context_message(session, &assistant_msg).await?;

            // Maybe store memory.
            if let Err(e) = self.memory_manager.maybe_store(session, &final_text).await {
                warn!(error = %e, "failed to auto-store memory");
            }

            return Ok(IterationOutcome::Final(OutgoingMessage {
                session_id: session.id.to_string(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                content: final_blocks,
                reply_to: None,
                metadata: Default::default(),
            }));
        }

        // Append assistant message including thinking and tool-call
        // blocks so the LLM sees its own prior reasoning and tool
        // invocations on the next turn.
        let mut assistant_blocks = if response.content_blocks.is_empty() {
            // Fallback: build from the flat content string.
            if response.content.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(response.content.clone())]
            }
        } else {
            response.content_blocks.clone()
        };
        for tc in &response.tool_calls {
            let block = ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
                signature: tc.signature.clone(),
            };
            assistant_blocks.push(block.clone());
            accumulated_tool_uses.push(block);
        }
        let assistant_msg = ChatMessage {
            role: Role::Assistant,
            content: assistant_blocks,
        };
        self.append_context_message(session, &assistant_msg).await?;

        // Execute tool calls. Approved resources are shared via a Mutex
        // so concurrent tool calls (when supported) see each other's
        // grants immediately. Wrapped in an `Arc` so that any
        // persist-always closure injected into `ToolContext`
        // mid-execution can clone its handle into the executor boundary
        // without a borrow-lifetime escape.
        let approved = std::sync::Arc::new(parking_lot::Mutex::new(
            session.state.approved_resources.clone(),
        ));

        // `spawn_subagent` requests are deferred until *after* the
        // enclosing LlmIteration step closes. Per `trace.md`, steps
        // cannot nest — the subagent's own `StepKind::Subagent` step
        // must run as a peer of this iteration's, not inside it. The
        // deferred dispatch order matches the LLM's tool_use order, so
        // the next iteration sees results in the same sequence the
        // model produced.
        let mut deferred_subagents: Vec<(String, serde_json::Value)> = Vec::new();

        for tool_call in &response.tool_calls {
            debug!(
                tool = %tool_call.name,
                "executing tool call"
            );

            if tool_call.name == SPAWN_SUBAGENT_TOOL_NAME {
                deferred_subagents.push((tool_call.id.clone(), tool_call.arguments.clone()));
                continue;
            }

            let tool_result = self
                .tool_executor
                .execute(
                    &tool_call.name,
                    tool_call.arguments.clone(),
                    &session.id,
                    &session.user,
                    &approved,
                    span_recorder,
                    &step,
                    Some(llm_span_id),
                    tool_call.id.clone(),
                    None,
                    Some(job_id),
                    cancel_token.child_token(),
                    notifier.clone(),
                )
                .await;

            let mut llm_visible_images: Vec<ContentBlock> = Vec::new();
            let raw_result_text = match &tool_result {
                Ok(ToolOutput::Text(s)) => s.clone(),
                Ok(ToolOutput::Json(v)) => {
                    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
                }
                Ok(ToolOutput::WithAttachments { text, attachments }) => {
                    push_bounded(accumulated_attachments, attachments.iter().cloned());
                    text.clone()
                }
                Ok(ToolOutput::MultiModalText { text, llm_images }) => {
                    // LLM-visible images go in BOTH directions: a
                    // follow-up User-role message (so the next turn
                    // sees them through the standard multimodal user
                    // path) AND the final OutgoingMessage (so the user
                    // channel renders them too).
                    push_bounded(accumulated_attachments, llm_images.iter().cloned());
                    llm_visible_images.extend(llm_images.iter().cloned());
                    text.clone()
                }
                Ok(ToolOutput::Error(msg)) => format!("Error: {msg}"),
                Err(e) => {
                    if let Some(denied) = e.downcast_ref::<aura_tools::ToolError>()
                        && matches!(denied, aura_tools::ToolError::Denied { .. })
                    {
                        format!(
                            "The user explicitly denied permission for tool '{}'. \
                             Do NOT retry this tool call. Either use an alternative \
                             approach or inform the user that the operation was skipped.",
                            tool_call.name
                        )
                    } else {
                        format!("Error: {e}")
                    }
                }
            };

            // Cap size before wrapping so the truncation notice lands
            // inside the `<tool_output>` envelope, then wrap so the LLM
            // sees a clear boundary around untrusted tool output.
            let capped = self.security_gateway.cap_tool_output(raw_result_text).await;
            let wrapped = self
                .security_gateway
                .wrap_tool_output_for_llm(&tool_call.name, &capped);

            // Append tool result to context with the tool_use_id so the
            // LLM can correlate results with their originating calls.
            let tool_msg = ChatMessage {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: tool_call.id.clone(),
                    content: wrapped,
                }],
            };
            self.append_context_message(session, &tool_msg).await?;

            // ToolResult.content is text-only; provider adapters
            // serialize it as plain text. To get images back into the
            // LLM's view, follow with a User-role message that carries
            // the same images plus a marker tying them to this tool
            // call. Vision-capable providers fetch the blob bytes via
            // the existing user_content_for_block path; non-vision
            // providers fall back to a text stub.
            if !llm_visible_images.is_empty() {
                let mut content: Vec<ContentBlock> =
                    Vec::with_capacity(llm_visible_images.len() + 1);
                content.push(ContentBlock::Text(format!(
                    "[image attachment(s) returned by tool `{}` (tool_use_id={})]",
                    tool_call.name, tool_call.id
                )));
                content.extend(llm_visible_images);
                let image_msg = ChatMessage {
                    role: Role::User,
                    content,
                };
                self.append_context_message(session, &image_msg).await?;
            }
        }

        // Flush accumulated approvals back into session state.
        session.state.approved_resources = approved.lock().clone();

        Ok(IterationOutcome::Continue { deferred_subagents })
    }

    /// Call the LLM with retry on transient errors using `ErrorHandler`.
    /// Returns the response paired with the `SpanId` of the
    /// **last attempt's** `LlmCall` span — that's the span tools spawned
    /// from this response should pair back to via `ToolCallOrigin`.
    async fn call_llm_with_retry(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        step: &StepHandle,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<(LlmResponse, aura_model::SpanId)> {
        let mut attempt = 0u32;
        loop {
            // Re-check before *every* attempt, not just the first.
            // Streaming partial-usage is billed via record_call even
            // when the call ends in Err, so a retry past a cap-breach
            // would silently keep accumulating spend without this gate.
            if let Some(cm) = &self.cost_manager {
                cm.check().map_err(|e| anyhow::anyhow!(e))?;
            }
            match self.call_llm(session, span_recorder, step, delta_tx).await {
                Ok(pair) => return Ok(pair),
                Err(e) => {
                    if !self.error_handler.should_retry(attempt, &e) {
                        return Err(e);
                    }
                    let backoff = self.error_handler.backoff_duration(attempt);
                    warn!(
                        attempt = attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %e,
                        "retrying LLM call after transient error"
                    );
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Call the LLM with the current session context. Opens an
    /// `LlmCall` span inside `step`, fills it with the response (or
    /// failure) before closing. Cost recording happens automatically:
    /// `SpanRecorder::end_span(LlmCall { tokens })` publishes
    /// `TraceEvent::LlmSpanEnded` for the cost subscriber.
    async fn call_llm(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        step: &StepHandle,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<(LlmResponse, aura_model::SpanId)> {
        let model_info = self.llm_client.model_info();

        let tool_defs: Vec<ToolDefinitionForLlm> = self
            .tool_registry
            .tool_definitions()
            .into_iter()
            .map(|td| ToolDefinitionForLlm {
                name: td.name,
                description: td.description,
                parameters_schema: td.parameters_schema,
            })
            .collect();

        let request = ChatRequest {
            messages: session.messages.clone(),
            temperature: None,
            tools: tool_defs,
        };

        crate::scope::with_llm_span(
            span_recorder.as_ref(),
            step,
            step.job_id,
            LlmCallBegin {
                model_id: model_info.id.clone(),
                provider: model_info.provider.clone(),
                provider_config_hash: String::new(),
                input_messages: session.messages.clone(),
                temperature: None,
            },
            None,
            |span| async move {
                let started_at = std::time::Instant::now();
                let (partial_usage, llm_result) = match delta_tx {
                    Some(tx) => self.chat_streaming(&request, session, tx).await,
                    None => match self.llm_client.chat(&request).await {
                        Ok(r) => (r.usage, Ok(r)),
                        Err(e) => (TokenUsage::default(), Err(e)),
                    },
                };
                let latency_ms = started_at.elapsed().as_millis() as u64;

                let (finalize, value_result) = match llm_result {
                    Ok(mut response) => {
                        // Defensive scrub of LLM output.
                        if let Err(e) = self
                            .security_gateway
                            .sanitize_llm_response(&mut response)
                            .await
                        {
                            warn!(error = %e, "failed to sanitize LLM response");
                        }
                        self.write_session_log(
                            session,
                            &request,
                            LlmCallOutcome::Ok {
                                response: LlmResponseMeta::from_response(&response),
                                latency_ms,
                            },
                        )
                        .await;
                        let trace_tool_calls: Vec<aura_trace::LlmToolCallRecord> = response
                            .tool_calls
                            .iter()
                            .map(|tc| aura_trace::LlmToolCallRecord {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            })
                            .collect();
                        let finalize = LlmCallResult {
                            output_content: response.content.clone(),
                            thinking: response.thinking.clone(),
                            tool_calls: trace_tool_calls,
                            input_tokens: response.usage.input_tokens,
                            output_tokens: response.usage.output_tokens,
                            cached_input_tokens: response.usage.cached_input_tokens,
                            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
                        };
                        (finalize, Ok((response, span.span_id)))
                    }
                    Err(e) => {
                        // Surface the *sanitized* message: this Err
                        // bubbles into `with_llm_span`, which writes
                        // `e.to_string()` into the span's
                        // `Failed { reason }` (via `outcome_for`) and
                        // from there into persisted trace storage.
                        // Returning the raw `e` would leak any secrets
                        // in the upstream provider error text.
                        let raw = e.to_string();
                        let error_msg = self
                            .security_gateway
                            .sanitize_error(&raw)
                            .await
                            .unwrap_or(raw);
                        self.write_session_log(
                            session,
                            &request,
                            LlmCallOutcome::Err {
                                error: error_msg.clone(),
                                latency_ms,
                            },
                        )
                        .await;
                        // Bill the partial-stream tokens so a failed
                        // LLM call still leaves a `cost_records` row
                        // — operators see the attempt rather than
                        // silently under-counting.
                        let finalize = LlmCallResult {
                            output_content: String::new(),
                            thinking: None,
                            tool_calls: Vec::new(),
                            input_tokens: partial_usage.input_tokens,
                            output_tokens: partial_usage.output_tokens,
                            cached_input_tokens: partial_usage.cached_input_tokens,
                            cache_creation_input_tokens: partial_usage.cache_creation_input_tokens,
                        };
                        (finalize, Err(anyhow::anyhow!(error_msg)))
                    }
                };

                // Memory-first, disk-second: in-memory budget state
                // updates synchronously here so the next iteration's
                // `check()` sees this spend; persistence is fire-and-
                // forget inside record_call.
                if let Some(cm) = &self.cost_manager {
                    cm.record_call(
                        &session.user.id,
                        session.id.clone(),
                        step.job_id,
                        span.span_id,
                        &model_info.id,
                        finalize.input_tokens,
                        finalize.output_tokens,
                        finalize.cached_input_tokens,
                        finalize.cache_creation_input_tokens,
                    );
                }

                (finalize, value_result)
            },
        )
        .await
    }

    /// Append a single LLM call record to the per-session JSONL log.
    /// No-op when no logger is configured. Failures are logged at warn
    /// and swallowed — log persistence must never block a turn.
    async fn write_session_log(
        &self,
        session: &Session,
        request: &ChatRequest,
        outcome: LlmCallOutcome,
    ) {
        let Some(logger) = self.session_log.as_ref() else {
            return;
        };
        let info = self.llm_client.model_info();
        let request = match LlmRequestMeta::from_request(request) {
            Ok(meta) => meta,
            Err(e) => {
                warn!(error = %e, "failed to summarize llm request for session log");
                return;
            }
        };
        let record = LlmCallRecord {
            timestamp: chrono::Utc::now(),
            session_id: session.id.to_string(),
            provider: info.provider.clone(),
            model: info.id.clone(),
            request,
            outcome,
        };
        if let Err(e) = logger.log_llm_call(&record).await {
            warn!(error = %e, "failed to append session llm log");
        }
    }

    async fn append_context_message(
        &mut self,
        session: &mut Session,
        message: &ChatMessage,
    ) -> anyhow::Result<()> {
        self.context_manager.append(session, message).await?;
        self.write_session_message_log(session, message).await;
        Ok(())
    }

    async fn insert_session_message(
        &self,
        session: &mut Session,
        index: usize,
        message: ChatMessage,
    ) {
        session.messages.insert(index, message.clone());
        self.write_session_message_log(session, &message).await;
    }

    async fn write_session_message_log(&self, session: &Session, message: &ChatMessage) {
        let Some(logger) = self.session_log.as_ref() else {
            return;
        };
        if let Err(e) = logger.log_message(session.id.as_str(), message).await {
            warn!(error = %e, "failed to append session message log");
        }
    }

    /// Run a streaming chat request, forwarding each text chunk through
    /// `delta_tx` while accumulating the full response to return.
    ///
    /// The delta stream is the single place where real plaintext may
    /// legitimately leave the agent: each chunk is scanned for leaks
    /// (tokenized into the placeholder form that the accumulated
    /// `content` remembers) and then revealed via the vault just before
    /// being sent to the adapter. The returned `LlmResponse.content`
    /// remains in placeholder form so trace / memory / next-turn context
    /// never see real secrets.
    ///
    /// Returns `(TokenUsage, Result)` so partial usage seen before a
    /// stream error still bills. Providers that only emit usage in
    /// the terminal `Final` event (today: OpenAI / Anthropic via rig)
    /// yield `TokenUsage::default()` on a mid-stream drop — the row
    /// still lands in `cost_records` with zero counts so operators
    /// see the failed call instead of silent under-billing.
    async fn chat_streaming(
        &self,
        request: &ChatRequest,
        session: &Session,
        delta_tx: &mpsc::Sender<AgentOutput>,
    ) -> (TokenUsage, aura_llm::Result<LlmResponse>) {
        let mut stream = match self.llm_client.chat_stream(request).await {
            Ok(s) => s,
            Err(e) => return (TokenUsage::default(), Err(e)),
        };
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = TokenUsage::default();
        let mut thinking = String::new();
        let mut thinking_blocks: Vec<ContentBlock> = Vec::new();

        // Buffer for a trailing fragment that might be the start of a
        // placeholder (e.g. the chunk ends in "[{REDACTED_S"). We hold
        // it back until a safe boundary is seen.
        let mut pending = String::new();

        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => return (usage, Err(e)),
            };
            match event {
                StreamEvent::Text(chunk) => {
                    pending.push_str(&chunk);

                    let flush_to = safe_flush_boundary(&pending);
                    if flush_to > 0 {
                        let flushable: String = pending.drain(..flush_to).collect();
                        self.stream_emit(&flushable, session, &mut content, delta_tx)
                            .await;
                    }
                }
                StreamEvent::ToolCall(info) => tool_calls.push(info),
                StreamEvent::Reasoning(r) => thinking.push_str(&r),
                StreamEvent::ThinkingBlock(block) => thinking_blocks.push(block),
                StreamEvent::Usage(u) => usage = u,
            }
        }

        // Flush any remaining buffered text.
        if !pending.is_empty() {
            let flushable = std::mem::take(&mut pending);
            self.stream_emit(&flushable, session, &mut content, delta_tx)
                .await;
        }

        // Build content_blocks: thinking blocks first (providers expect
        // them before text), then text.
        let mut content_blocks = thinking_blocks;
        if !content.is_empty() {
            content_blocks.push(ContentBlock::Text(content.clone()));
        }

        (
            usage,
            Ok(LlmResponse {
                content,
                content_blocks,
                tool_calls,
                usage,
                thinking: if thinking.is_empty() {
                    None
                } else {
                    Some(thinking)
                },
            }),
        )
    }

    /// Tokenize a single stream fragment:
    /// scan for leaks, mint placeholders, persist to vault, substitute. The
    /// placeholder-form is both appended to `content` (so the accumulated
    /// `LlmResponse` stays sanitized) and delivered as-is to the adapter, so
    /// the streaming view matches the final persisted message.
    async fn stream_emit(
        &self,
        fragment: &str,
        session: &Session,
        content: &mut String,
        delta_tx: &mpsc::Sender<AgentOutput>,
    ) {
        let sanitized = match self
            .security_gateway
            .sanitize_stream_fragment(fragment)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to sanitize stream fragment; dropping");
                return;
            }
        };

        content.push_str(&sanitized);

        if delta_tx
            .send(AgentOutput::Delta {
                session_id: session.id.to_string(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                text: sanitized,
            })
            .await
            .is_err()
        {
            debug!("delta receiver dropped, continuing without forwarding");
        }
    }

    /// Dispatch one `spawn_subagent` tool_use through the subagent
    /// runtime. Opens a dedicated `StepKind::Subagent` step + a
    /// single `SubagentStub` span that bounds the parent's wait
    /// window. Returns the rendered tool_result text the parent's
    /// next LLM iteration will see.
    async fn dispatch_subagent(
        &self,
        session: &Session,
        job_id: JobId,
        span_recorder: &Arc<SpanRecorder>,
        arguments: serde_json::Value,
        parent_cancel: CancellationToken,
    ) -> anyhow::Result<String> {
        let runtime = match self.subagent_runtime.as_ref() {
            Some(rt) => Arc::clone(rt),
            None => {
                return Ok(SubagentResult {
                    child_session_id: aura_model::SessionId::from(""),
                    final_content: None,
                    status: SubagentExitStatus::Failed("no subagent runtime registered".into()),
                }
                .to_tool_result_text());
            }
        };
        let mut request = parse_spawn_request(&arguments).map_err(|e| anyhow::anyhow!(e))?;

        // Q10 A3: prepend a parent-conversation summary so the child
        // has provenance without inheriting the full transcript.
        // Failures degrade silently.
        match self
            .summarize_for_subagent(session, span_recorder, job_id)
            .await
        {
            Ok(Some(summary)) => {
                request
                    .must_include_context
                    .insert(0, format!("Parent conversation summary:\n{summary}"));
            }
            Ok(None) => {}
            Err(e) => {
                warn!(error = %e, "subagent context summarization failed; spawning without it")
            }
        }

        // Prepare the child synchronously *before* opening the
        // Subagent step so the step kind carries the real
        // child_session_id instead of an empty placeholder.
        let prepared = match runtime.prepare(session, job_id, &request).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(SubagentResult {
                    child_session_id: aura_model::SessionId::from(""),
                    final_content: None,
                    status: SubagentExitStatus::Failed(e),
                }
                .to_tool_result_text());
            }
        };
        let child_session_id = prepared.child_session.id.clone();

        // Real parent token from the agent loop's cancel tree, not a
        // throwaway. The runtime derives a child_token() from this for
        // its own bookkeeping; tripping our parent (via JobLifecycle::cancel)
        // cascades into every nested subagent.
        let result_text = crate::scope::with_step(
            span_recorder.as_ref(),
            job_id,
            StepKind::Subagent {
                child_session_id: child_session_id.clone(),
            },
            None,
            |step| async move {
                crate::scope::with_span(
                    span_recorder.as_ref(),
                    &step,
                    job_id,
                    SpanKind::SubagentStub {
                        child_session_id: child_session_id.clone(),
                    },
                    None,
                    None,
                    |_span| async move {
                        let result = runtime.run(prepared, request, parent_cancel).await;
                        let outcome = match &result.status {
                            SubagentExitStatus::Completed => LifecycleOutcome::Ok,
                            SubagentExitStatus::Cancelled => LifecycleOutcome::Cancelled {
                                reason: aura_job::CancelReason::ParentCancelled,
                            },
                            SubagentExitStatus::Failed(reason) => LifecycleOutcome::Failed {
                                reason: reason.clone(),
                            },
                            SubagentExitStatus::Timeout => LifecycleOutcome::Cancelled {
                                reason: aura_job::CancelReason::SubagentTimeout,
                            },
                        };
                        Ok((SpanFinalize::Empty, outcome.clone(), (outcome, result)))
                    },
                )
                .await
                .map(|(outcome, result)| (outcome, result.to_tool_result_text()))
            },
        )
        .await?;

        Ok(result_text)
    }

    /// Condense the parent's recent non-system messages into a summary
    /// for a child subagent, wrapped in a Compression step / LlmCall
    /// span. Returns `Ok(None)` for brand-new sessions or empty LLM
    /// output — the subagent still launches.
    async fn summarize_for_subagent(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
    ) -> anyhow::Result<Option<String>> {
        let has_real_history = session
            .messages
            .iter()
            .any(|m| matches!(m.role, Role::Assistant | Role::Tool));
        if !has_real_history {
            return Ok(None);
        }

        let model_info = self.llm_client.model_info();
        let summarize_prompt = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(
                "Summarize the prior conversation into a concise briefing for a subagent. \
                 Focus on the user's current goal, decisions already made, \
                 outstanding questions, and constraints. Output plain prose, \
                 no preamble."
                    .to_string(),
            )],
        };
        let mut messages: Vec<ChatMessage> = session
            .messages
            .iter()
            .filter(|m| !matches!(m.role, Role::System))
            .cloned()
            .collect();
        messages.push(summarize_prompt);

        let llm_call_kind = SpanKind::LlmCall {
            begin: LlmCallBegin {
                model_id: model_info.id.clone(),
                provider: model_info.provider.clone(),
                provider_config_hash: String::new(),
                input_messages: messages.clone(),
                temperature: None,
            },
            result: None,
        };

        let rec = span_recorder.as_ref();
        let llm = &self.llm_client;
        let content = crate::scope::with_step(
            rec,
            job_id,
            StepKind::Compression,
            None,
            |step| async move {
                let v = crate::scope::with_span(
                    rec,
                    &step,
                    job_id,
                    llm_call_kind,
                    None,
                    None,
                    |_span| async move {
                        let request = ChatRequest {
                            messages,
                            temperature: None,
                            tools: Vec::new(),
                        };
                        let response = llm.chat(&request).await?;
                        let finalize = SpanFinalize::LlmCall(LlmCallResult {
                            output_content: response.content.clone(),
                            thinking: response.thinking.clone(),
                            tool_calls: vec![],
                            input_tokens: response.usage.input_tokens,
                            output_tokens: response.usage.output_tokens,
                            cached_input_tokens: response.usage.cached_input_tokens,
                            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
                        });
                        Ok((finalize, LifecycleOutcome::Ok, response.content))
                    },
                )
                .await?;
                Ok((LifecycleOutcome::Ok, v))
            },
        )
        .await?;

        let trimmed = content.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Compress if the budget calls for it. When the strategy ran an
    /// LLM call, brackets it in a `StepKind::Compression` step +
    /// `SpanKind::LlmCall` span so the cost lands in per-step
    /// aggregation. Strategies that don't call an LLM (e.g. `Truncate`)
    /// don't open a step.
    async fn compress_if_needed(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        cancel_token: &CancellationToken,
    ) -> anyhow::Result<()> {
        let outcome = self.context_manager.maybe_compress(session).await?;
        let Some(stats) = outcome else {
            return Ok(());
        };
        debug!(
            before = stats.before_tokens,
            after = stats.after_tokens,
            "compressed context before LLM call"
        );
        let Some(call) = stats.llm_call else {
            return Ok(());
        };

        // Post-hoc record — `SpanRecorder::end_span` publishes
        // `LlmSpanEnded` for the cost subscriber regardless of
        // wall-clock ordering.
        let llm_call_kind = SpanKind::LlmCall {
            begin: LlmCallBegin {
                model_id: call.model_id,
                provider: call.provider,
                provider_config_hash: String::new(),
                input_messages: Vec::new(),
                temperature: None,
            },
            result: None,
        };
        let finalize = SpanFinalize::LlmCall(LlmCallResult {
            output_content: String::new(),
            thinking: None,
            tool_calls: vec![],
            input_tokens: call.input_tokens,
            output_tokens: call.output_tokens,
            cached_input_tokens: call.cached_input_tokens,
            cache_creation_input_tokens: call.cache_creation_input_tokens,
        });
        let cancel_ctx = Some((cancel_token, aura_job::CancelReason::ParentCancelled));
        let rec = span_recorder.as_ref();
        crate::scope::with_step(
            rec,
            job_id,
            StepKind::Compression,
            cancel_ctx,
            |step| async move {
                crate::scope::with_span(
                    rec,
                    &step,
                    job_id,
                    llm_call_kind,
                    None,
                    cancel_ctx,
                    |_span| async move { Ok((finalize, LifecycleOutcome::Ok, ())) },
                )
                .await?;
                Ok((LifecycleOutcome::Ok, ()))
            },
        )
        .await
    }

    async fn ensure_system_prompt(&self, session: &mut Session) {
        let has_system = session
            .messages
            .first()
            .is_some_and(|m| m.role == Role::System);
        if !has_system {
            let msg = ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(self.soul.system_prompt().to_string())],
            };
            self.insert_session_message(session, 0, msg).await;
        }
    }

    fn invocable_skills(&self) -> Vec<SkillSummary> {
        self.skill_registry
            .all_summaries_sorted()
            .into_iter()
            .filter(|s| {
                s.agent_invocable && !matches!(s.trust_level, aura_model::TrustLevel::Untrusted)
            })
            .collect()
    }
}

fn build_skill_reminder(skills: &[SkillSummary]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut s =
        String::from("Available skills (invoke via the `Skill` tool with `skill: \"<name>\"`):\n");
    for sk in skills {
        let cmd = sk.command.as_deref().unwrap_or(sk.name.as_str());
        s.push_str(&format!("- /{cmd}: {}", sk.description.trim()));
        if let Some(hint) = sk.argument_hint.as_deref() {
            s.push_str(&format!(" {hint}"));
        }
        s.push('\n');
    }
    Some(s)
}

fn detect_slash_invocation(user_text: &str, skills: &[SkillSummary]) -> Option<(String, String)> {
    let rest = user_text.trim_start().strip_prefix('/')?;
    let (cmd, args) = match rest.find(char::is_whitespace) {
        Some(idx) => (&rest[..idx], rest[idx..].trim().to_string()),
        None => (rest, String::new()),
    };
    if cmd.is_empty() {
        return None;
    }
    let skill = skills.iter().find(|s| s.command.as_deref() == Some(cmd))?;
    Some((skill.name.clone(), args))
}

#[cfg(test)]
mod notifier_bridge_tests {
    use super::*;
    use aura_channels::AgentOutput;
    use aura_tools::{NoticeLevel as ToolsNoticeLevel, SessionNotifier};

    fn mk_notifier() -> (DeltaTxNotifier, tokio::sync::mpsc::Receiver<AgentOutput>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let n = DeltaTxNotifier {
            tx,
            session_id: "s".into(),
            user_id: "u".into(),
            channel: aura_model::ChannelType::tui(),
        };
        (n, rx)
    }

    #[test]
    fn warn_forwards_as_agent_output_warn() {
        let (n, mut rx) = mk_notifier();
        n.emit(ToolsNoticeLevel::Warn, "summary", "detail");
        let out = rx.try_recv().expect("notice should be queued");
        match out {
            AgentOutput::Notice {
                level,
                text,
                session_id,
                user_id,
                ..
            } => {
                assert_eq!(level, aura_channels::NoticeLevel::Warn);
                assert_eq!(text, "summary: detail");
                assert_eq!(session_id, "s");
                assert_eq!(user_id, "u");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn error_forwards_as_agent_output_error() {
        let (n, mut rx) = mk_notifier();
        n.emit(ToolsNoticeLevel::Error, "blocked", "rationale");
        match rx.try_recv().unwrap() {
            AgentOutput::Notice { level, text, .. } => {
                assert_eq!(level, aura_channels::NoticeLevel::Error);
                assert_eq!(text, "blocked: rationale");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn empty_detail_collapses_text() {
        let (n, mut rx) = mk_notifier();
        n.emit(ToolsNoticeLevel::Warn, "headline", "");
        match rx.try_recv().unwrap() {
            AgentOutput::Notice { text, .. } => assert_eq!(text, "headline"),
            _ => panic!(),
        }
    }

    #[test]
    fn full_channel_drops_silently() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let n = DeltaTxNotifier {
            tx,
            session_id: "s".into(),
            user_id: "u".into(),
            channel: aura_model::ChannelType::tui(),
        };
        n.emit(ToolsNoticeLevel::Warn, "first", "");
        // Second emit should not block or panic — try_send drops it.
        n.emit(ToolsNoticeLevel::Warn, "second", "");
        // Only the first one is in the queue.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod stream_buffer_tests {
    use super::safe_flush_boundary;

    #[test]
    fn empty_flushes_nothing() {
        assert_eq!(safe_flush_boundary(""), 0);
    }

    #[test]
    fn plain_text_flushes_all() {
        let s = "hello world";
        assert_eq!(safe_flush_boundary(s), s.len());
    }

    #[test]
    fn trailing_open_bracket_is_withheld() {
        let s = "hello [";
        assert_eq!(safe_flush_boundary(s), s.find('[').unwrap());
    }

    #[test]
    fn trailing_partial_placeholder_is_withheld() {
        let s = "abc [{REDACTED_SECRET_deadbee";
        assert_eq!(safe_flush_boundary(s), s.find('[').unwrap());
    }

    #[test]
    fn complete_placeholder_flushes_all() {
        let s = "abc [{REDACTED_SECRET_deadbeefdeadbeefdeadbeef}] def";
        assert_eq!(safe_flush_boundary(s), s.len());
    }

    #[test]
    fn high_water_forces_flush() {
        let s: String = "a".repeat(200) + "[";
        assert_eq!(safe_flush_boundary(&s), s.len());
    }
}

#[cfg(test)]
mod attachment_bound_tests {
    use super::{MAX_ATTACHMENTS_PER_TURN, push_bounded};
    use aura_model::{BlobRef, ContentBlock};

    fn img(tag: &str) -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: format!("sha256:{tag}"),
            },
            mime_type: "image/png".into(),
        }
    }

    #[test]
    fn under_cap_keeps_everything() {
        let mut v: Vec<ContentBlock> = Vec::new();
        push_bounded(&mut v, vec![img("a"), img("b"), img("c")]);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn over_cap_evicts_oldest_first() {
        let mut v: Vec<ContentBlock> = Vec::new();
        let pushed: Vec<ContentBlock> = (0..MAX_ATTACHMENTS_PER_TURN + 5)
            .map(|i| img(&format!("{i:0>2}")))
            .collect();
        push_bounded(&mut v, pushed);
        assert_eq!(v.len(), MAX_ATTACHMENTS_PER_TURN);
        // Newest 16 survive; the first 5 (00..04) evicted.
        match &v[0] {
            ContentBlock::Image { blob, .. } => {
                assert!(
                    blob.blob_id.ends_with("05"),
                    "oldest survivor should be 05, got {}",
                    blob.blob_id
                );
            }
            _ => panic!("expected image"),
        }
        match v.last().unwrap() {
            ContentBlock::Image { blob, .. } => {
                let last = format!("{:0>2}", MAX_ATTACHMENTS_PER_TURN + 4);
                assert!(
                    blob.blob_id.ends_with(&last),
                    "newest should be {last}, got {}",
                    blob.blob_id
                );
            }
            _ => panic!("expected image"),
        }
    }
}
