use std::sync::Arc;
use std::time::Duration;

use aura_channels::{AgentOutput, NoticeLevel, OutgoingMessage};
use aura_context::ContextManager;
use aura_hook::{HookAction, HookContext, HookEventData, HookManager, HookPoint};
use aura_job::{JobInput, JobOutput};
use aura_llm::{
    ChatRequest, LlmCompletion, LlmResponse, StreamEvent, TokenUsage, ToolDefinitionForLlm,
};
use aura_model::{ChatMessage, ContentBlock, JobId, Role};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::memory::MemoryManager;
use aura_model::Session;
use aura_model::{HookPhase, SpanId};
use aura_skills::SkillRegistry;
use aura_skills_assessor::{RiskLevel, SkillAssessor};
use aura_tools::{ToolOutput, ToolRegistry};
use aura_trace::{LifecycleOutcome, SpanEventKind, SpanKind, SpanResult, StepHandle, StepKind};
use tracing::{debug, info, warn};

use crate::error_recovery::ErrorHandler;
use crate::job::JobLifecycle;
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

/// Per-call timeout for `PreStep` / `PostStep` hooks. A hook that
/// overruns is treated as `Continue` and a `tracing::warn` is emitted.
const STEP_HOOK_TIMEOUT: Duration = Duration::from_millis(500);

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

/// Lazy `SpanKind::StepHost` anchor — `begin_span` is deferred to the
/// first `record_hook_degraded` so the no-degradation path (the common
/// case) doesn't pay two extra DB writes per iteration.
struct StepHostSpan<'a> {
    span_recorder: &'a Arc<SpanRecorder>,
    step: &'a StepHandle,
    handle: Option<aura_trace::SpanHandle>,
    seq: u32,
}

impl<'a> StepHostSpan<'a> {
    fn new(span_recorder: &'a Arc<SpanRecorder>, step: &'a StepHandle) -> Self {
        Self {
            span_recorder,
            step,
            handle: None,
            seq: 0,
        }
    }

    async fn record_hook_degraded(&mut self, hook_name: &str, phase: HookPhase) {
        let span_id = match self.ensure_open().await {
            Ok(id) => id,
            Err(e) => {
                warn!(error = %e, "failed to open host span for HookDegraded SpanEvent");
                return;
            }
        };
        let next = self.seq;
        match self
            .span_recorder
            .emit_event(
                span_id,
                next,
                SpanEventKind::HookDegraded {
                    hook_name: hook_name.to_string(),
                    timeout_ms: STEP_HOOK_TIMEOUT.as_millis() as u64,
                    phase,
                },
            )
            .await
        {
            Ok(()) => self.seq = next.wrapping_add(1),
            Err(e) => warn!(error = %e, "failed to persist HookDegraded SpanEvent"),
        }
    }

    async fn ensure_open(&mut self) -> Result<SpanId, aura_trace::TraceError> {
        if let Some(h) = &self.handle {
            return Ok(h.span_id);
        }
        let h = self
            .span_recorder
            .begin_span(self.step, SpanKind::StepHost, None)
            .await?;
        let id = h.span_id;
        self.handle = Some(h);
        Ok(id)
    }

    async fn close(self, job_id: JobId, outcome: LifecycleOutcome) {
        if let Some(h) = self.handle {
            self.span_recorder
                .end_span(h, job_id, SpanResult::StepHost, outcome)
                .await
                .ok();
        }
    }
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

/// Outcome of the skill risk assessor for a single candidate. `run()`
/// dispatches on this — `Block` drops the skill with an error notice,
/// `PassWithWarning` keeps it with a warning notice, and `Pass` is
/// silent.
enum SkillGate {
    Pass,
    PassWithWarning { rationale: String },
    Block { rationale: String },
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
    /// Step boundary hook manager. Drives `PreStep` / `PostStep` with
    /// the timeout / degraded protocol. Optional so test harnesses
    /// that don't care about hooks can leave it unset.
    hooks: Option<Arc<HookManager>>,
    /// Optional subagent runtime. When set, LLM `tool_use` calls
    /// targeting `spawn_subagent` short-circuit the regular
    /// tool_executor path and route through here. Unset →
    /// `spawn_subagent` falls back to the regular tool catalogue (and
    /// today returns an unknown-tool error).
    subagent_runtime: Option<Arc<dyn SubagentRuntime>>,
    /// Optional LLM risk assessor. When set, every skill candidate is
    /// checked before injection: `Dangerous` verdicts veto the skill,
    /// `Suspicious` verdicts log a warning but allow it through.
    skill_assessor: Option<Arc<SkillAssessor>>,
    /// Optional per-session JSONL logger for LLM calls. When set, every
    /// `call_llm` invocation appends a record (request, response or
    /// error, latency, model metadata) to
    /// `<state>/sessions/<session_id>.jsonl`.
    session_log: Option<Arc<SessionLlmLogger>>,
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
            hooks: None,
            subagent_runtime: None,
            skill_assessor: None,
            session_log: None,
        }
    }

    /// Attach the step-boundary hook manager.
    pub fn with_hooks(mut self, hooks: Arc<HookManager>) -> Self {
        self.hooks = Some(hooks);
        self
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
        let job = job_lifecycle
            .start_job(
                session.id.clone(),
                session.trigger.kind(),
                job_input,
                session.bound_soul_version.clone(),
                parent_job_id,
            )
            .await?;
        let job_id = job.id;
        // Register the in-flight token before transitioning to
        // InProgress so `JobLifecycle::cancel` can find us. The
        // guard's Drop runs after `run_inner` returns / panics /
        // unwinds, so the registry stays consistent on every path.
        let _cancel_guard = job_lifecycle.register_running(job_id, cancel_token.clone());
        job_lifecycle.start(&job_id).await?;

        match self
            .run_inner(
                session,
                user_content,
                job_lifecycle,
                span_recorder,
                job_id,
                delta_tx,
                cancel_token.clone(),
            )
            .await
        {
            Ok(outgoing) => {
                if let Err(e) = job_lifecycle
                    .complete(
                        &job_id,
                        JobOutput::Message {
                            content: outgoing.content.clone(),
                        },
                    )
                    .await
                {
                    warn!(error = %e, "failed to mark job complete");
                }
                Ok(outgoing)
            }
            Err(e) => {
                if let Err(fe) = job_lifecycle.fail(&job_id, e.to_string()).await {
                    warn!(error = %fe, "failed to mark job failed");
                }
                Err(e)
            }
        }
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

        // Skill selection: `SkillRegistry::select` returns exactly the
        // one skill invoked by `/<cmd>`, or the full registered set
        // otherwise. We inject every returned candidate after the risk
        // assessor clears it — narrowing already happened upstream.
        //
        // Risk gating: `Dangerous` drops the skill with an error
        // notice, `Suspicious` keeps it with a warn notice. Slash
        // invocations were explicit, and the full-set fall-through is
        // also a user-visible action (they opened the chat with this
        // registry loaded), so in both cases a non-Safe verdict is
        // surfaced via `AgentOutput::Notice` rather than hidden.
        let user_text = aura_llm::multimodal::extract_text(&user_content);
        let skill_candidates = self.skill_registry.select(&user_text);

        let mut active_skills: Vec<aura_skills::SkillDefinition> = Vec::new();
        for candidate in skill_candidates.into_iter() {
            match self.assess_skill_risk(&candidate.skill).await {
                SkillGate::Pass => active_skills.push(candidate.skill),
                SkillGate::PassWithWarning { rationale } => {
                    self.emit_skill_notice(
                        session,
                        NoticeLevel::Warn,
                        &candidate.skill.name,
                        "rated suspicious",
                        &rationale,
                        delta_tx.as_ref(),
                    )
                    .await;
                    active_skills.push(candidate.skill);
                }
                SkillGate::Block { rationale } => {
                    self.emit_skill_notice(
                        session,
                        NoticeLevel::Error,
                        &candidate.skill.name,
                        "blocked",
                        &rationale,
                        delta_tx.as_ref(),
                    )
                    .await;
                }
            }
        }

        for skill in &active_skills {
            let skill_msg = ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(aura_skills::render::render_skill_block(
                    skill,
                ))],
            };
            self.push_session_message(session, skill_msg).await;
        }

        session.state.active_skills = active_skills.iter().map(|s| s.name.clone()).collect();

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
            if iterations >= self.policy.max_iterations {
                warn!(max = self.policy.max_iterations, "max iterations reached");
                break;
            }
            iterations += 1;

            // Proactive compression before building the ChatRequest.
            self.compress_if_needed(session, span_recorder, job_id)
                .await?;

            let step = span_recorder
                .begin_step(job_id, StepKind::LlmIteration)
                .await?;
            let mut host = StepHostSpan::new(span_recorder, &step);

            if self
                .fire_pre_step(session, job_id, &StepKind::LlmIteration, &mut host)
                .await
                .is_err()
            {
                let aborted = LifecycleOutcome::Cancelled {
                    reason: aura_job::CancelReason::HookAborted,
                };
                host.close(job_id, aborted.clone()).await;
                span_recorder.end_step(step, aborted).await.ok();
                return Err(anyhow::anyhow!("step aborted by PreStep hook"));
            }

            // Call LLM with retry on transient errors. Deltas are only
            // streamed on the first iteration of the loop.
            let iter_delta_tx = if iterations == 1 {
                delta_tx.as_ref()
            } else {
                None
            };
            let llm_result = self
                .call_llm_with_retry(session, span_recorder, &step, iter_delta_tx)
                .await;
            let (response, llm_span_id) = match llm_result {
                Ok(pair) => pair,
                Err(e) => {
                    let failed = LifecycleOutcome::Failed {
                        reason: e.to_string(),
                    };
                    host.close(job_id, failed.clone()).await;
                    span_recorder.end_step(step, failed).await.ok();
                    return Err(e);
                }
            };

            // If no tool calls, we have the final response
            if response.tool_calls.is_empty() {
                let step_id_for_hook = step.step_id;
                self.fire_post_step(
                    session,
                    job_id,
                    step_id_for_hook,
                    &StepKind::LlmIteration,
                    &LifecycleOutcome::Ok,
                    &mut host,
                )
                .await;
                host.close(job_id, LifecycleOutcome::Ok).await;
                span_recorder
                    .end_step(step, LifecycleOutcome::Ok)
                    .await
                    .ok();
                // Use content_blocks when available, falling back to the text string.
                let response_blocks = if response.content_blocks.is_empty() {
                    vec![ContentBlock::Text(response.content.clone())]
                } else {
                    response.content_blocks.clone()
                };

                // Append the tool_use blocks issued during intermediate
                // iterations after the final narration so channels that
                // key off them (e.g. the TUI cron hint) can render below
                // the assistant's reply.
                let mut final_blocks = response_blocks.clone();
                final_blocks.extend(std::mem::take(&mut accumulated_tool_uses));
                final_blocks.extend(std::mem::take(&mut accumulated_attachments));

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

                // Maybe store memory
                if let Err(e) = self.memory_manager.maybe_store(session, &final_text).await {
                    warn!(error = %e, "failed to auto-store memory");
                }

                return Ok(OutgoingMessage {
                    session_id: session.id.to_string(),
                    user_id: session.user.id.clone(),
                    channel: session.channel.clone(),
                    content: final_blocks,
                    reply_to: None,
                    metadata: Default::default(),
                });
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

            // Execute tool calls. Approved resources are shared via a
            // Mutex so concurrent tool calls (when supported) see each
            // other's grants immediately. Wrapped in an `Arc` so that
            // any persist-always closure injected into `ToolContext`
            // mid-execution can clone its handle into the executor
            // boundary without a borrow-lifetime escape.
            let approved = std::sync::Arc::new(parking_lot::Mutex::new(
                session.state.approved_resources.clone(),
            ));

            // `spawn_subagent` requests are deferred until *after* the
            // enclosing LlmIteration step closes. Per `trace.md`, steps
            // cannot nest — the subagent's own `StepKind::Subagent` step
            // must run as a peer of this iteration's, not inside it.
            // The deferred dispatch order matches the LLM's tool_use
            // order, so the next iteration sees results in the same
            // sequence the model produced.
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
                    )
                    .await;

                let mut llm_visible_images: Vec<ContentBlock> = Vec::new();
                let raw_result_text = match &tool_result {
                    Ok(ToolOutput::Text(s)) => s.clone(),
                    Ok(ToolOutput::Json(v)) => {
                        serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
                    }
                    Ok(ToolOutput::WithAttachments { text, attachments }) => {
                        push_bounded(&mut accumulated_attachments, attachments.iter().cloned());
                        text.clone()
                    }
                    Ok(ToolOutput::MultiModalText { text, llm_images }) => {
                        // LLM-visible images go in BOTH directions: a
                        // follow-up User-role message (so the next turn
                        // sees them through the standard multimodal user
                        // path) AND the final OutgoingMessage (so the
                        // user channel renders them too).
                        push_bounded(&mut accumulated_attachments, llm_images.iter().cloned());
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
                // serialize it as plain text. To get images back into
                // the LLM's view, follow with a User-role message that
                // carries the same images plus a marker tying them to
                // this tool call. Vision-capable providers fetch the
                // blob bytes via the existing user_content_for_block
                // path; non-vision providers fall back to a text stub.
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

            let step_id_for_hook = step.step_id;
            self.fire_post_step(
                session,
                job_id,
                step_id_for_hook,
                &StepKind::LlmIteration,
                &LifecycleOutcome::Ok,
                &mut host,
            )
            .await;
            host.close(job_id, LifecycleOutcome::Ok).await;
            span_recorder
                .end_step(step, LifecycleOutcome::Ok)
                .await
                .ok();

            // Now that the iteration step is closed, dispatch any
            // deferred subagent calls as peer steps. Each invocation
            // appends a tool_result message into context so the next
            // iteration's LLM call sees the outcome.
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

        let span = span_recorder
            .begin_span(
                step,
                SpanKind::LlmCall {
                    model_id: model_info.id.clone(),
                    provider: model_info.provider.clone(),
                    provider_config_hash: String::new(),
                    input_messages: session.messages.clone(),
                    temperature: None,
                    output_content: String::new(),
                    thinking: None,
                    tool_calls: vec![],
                    input_tokens: 0,
                    output_tokens: 0,
                },
                None,
            )
            .await?;

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

        let started_at = std::time::Instant::now();
        let llm_result = match delta_tx {
            Some(tx) => self.chat_streaming(&request, session, tx).await,
            None => self.llm_client.chat(&request).await,
        };
        let latency_ms = started_at.elapsed().as_millis() as u64;

        match llm_result {
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

                let span_id = span.span_id;
                span_recorder
                    .end_span(
                        span,
                        step.job_id,
                        SpanResult::LlmCall {
                            output_content: response.content.clone(),
                            thinking: response.thinking.clone(),
                            tool_calls: trace_tool_calls,
                            input_tokens: response.usage.input_tokens,
                            output_tokens: response.usage.output_tokens,
                        },
                        LifecycleOutcome::Ok,
                    )
                    .await?;

                Ok((response, span_id))
            }
            Err(e) => {
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
                span_recorder
                    .end_span(
                        span,
                        step.job_id,
                        SpanResult::LlmCall {
                            output_content: String::new(),
                            thinking: None,
                            tool_calls: vec![],
                            input_tokens: 0,
                            output_tokens: 0,
                        },
                        LifecycleOutcome::Failed {
                            reason: error_msg.clone(),
                        },
                    )
                    .await?;
                Err(e.into())
            }
        }
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

    async fn push_session_message(&self, session: &mut Session, message: ChatMessage) {
        session.messages.push(message.clone());
        self.write_session_message_log(session, &message).await;
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
    async fn chat_streaming(
        &self,
        request: &ChatRequest,
        session: &Session,
        delta_tx: &mpsc::Sender<AgentOutput>,
    ) -> aura_llm::Result<LlmResponse> {
        let mut stream = self.llm_client.chat_stream(request).await?;
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = TokenUsage::default();
        let mut thinking = String::new();
        let mut thinking_blocks: Vec<ContentBlock> = Vec::new();

        // Buffer for a trailing fragment that might be the start of a
        // placeholder (e.g. the chunk ends in "{{SECR"). We hold it back
        // until a safe boundary is seen.
        let mut pending = String::new();

        while let Some(event) = stream.next().await {
            match event? {
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
        })
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
                    child_root_job_id: None,
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

        let subagent_step = span_recorder
            .begin_step(
                job_id,
                StepKind::Subagent {
                    child_session_id: aura_model::SessionId::from(""),
                    child_root_job_id: aura_model::JobId::new(),
                },
            )
            .await?;
        let stub_span = span_recorder
            .begin_span(
                &subagent_step,
                SpanKind::SubagentStub {
                    child_session_id: aura_model::SessionId::from(""),
                },
                None,
            )
            .await?;

        // Real parent token from the agent loop's cancel tree, not a
        // throwaway. The runtime derives a child_token() from this for
        // its own bookkeeping; tripping our parent (via JobLifecycle::cancel)
        // cascades into every nested subagent.
        let result = runtime.spawn(session, job_id, request, parent_cancel).await;

        // Close stub span + subagent step.
        let outcome = match &result.status {
            SubagentExitStatus::Completed => LifecycleOutcome::Ok,
            SubagentExitStatus::Cancelled => LifecycleOutcome::Cancelled {
                reason: aura_job::CancelReason::ParentCancelled,
            },
            SubagentExitStatus::Failed(reason) => LifecycleOutcome::Failed {
                reason: reason.clone(),
            },
            SubagentExitStatus::Timeout => LifecycleOutcome::Failed {
                reason: "subagent timeout".to_string(),
            },
        };
        span_recorder
            .end_span(
                stub_span,
                job_id,
                SpanResult::SubagentStub {
                    child_session_id: result.child_session_id.clone(),
                },
                outcome.clone(),
            )
            .await
            .ok();
        span_recorder.end_step(subagent_step, outcome).await.ok();

        Ok(result.to_tool_result_text())
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

        let step = span_recorder
            .begin_step(job_id, StepKind::Compression)
            .await?;
        let model_info = self.llm_client.model_info();

        // Build a fresh ChatRequest scoped to "summarize the prior
        // conversation". The user message carries explicit instructions
        // so the prompt stays self-describing without depending on the
        // soul's system prompt.
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

        let span = span_recorder
            .begin_span(
                &step,
                SpanKind::LlmCall {
                    model_id: model_info.id.clone(),
                    provider: model_info.provider.clone(),
                    provider_config_hash: String::new(),
                    input_messages: messages.clone(),
                    temperature: None,
                    output_content: String::new(),
                    thinking: None,
                    tool_calls: vec![],
                    input_tokens: 0,
                    output_tokens: 0,
                },
                None,
            )
            .await?;

        let request = ChatRequest {
            messages,
            temperature: None,
            tools: Vec::new(),
        };
        let result = self.llm_client.chat(&request).await;
        match result {
            Ok(response) => {
                span_recorder
                    .end_span(
                        span,
                        job_id,
                        SpanResult::LlmCall {
                            output_content: response.content.clone(),
                            thinking: response.thinking.clone(),
                            tool_calls: vec![],
                            input_tokens: response.usage.input_tokens,
                            output_tokens: response.usage.output_tokens,
                        },
                        LifecycleOutcome::Ok,
                    )
                    .await
                    .ok();
                span_recorder
                    .end_step(step, LifecycleOutcome::Ok)
                    .await
                    .ok();
                let trimmed = response.content.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Err(e) => {
                let reason = e.to_string();
                let failed = LifecycleOutcome::Failed {
                    reason: reason.clone(),
                };
                span_recorder
                    .end_span(
                        span,
                        job_id,
                        SpanResult::LlmCall {
                            output_content: String::new(),
                            thinking: None,
                            tool_calls: vec![],
                            input_tokens: 0,
                            output_tokens: 0,
                        },
                        failed.clone(),
                    )
                    .await
                    .ok();
                span_recorder.end_step(step, failed).await.ok();
                Err(e.into())
            }
        }
    }

    /// Returns `Err(())` only when the hook chain returns
    /// `HookAction::Abort` (the surrounding job is then cancelled by
    /// the caller). On `Block` or timeout we proceed (default-allow);
    /// a `tracing::warn` is emitted on timeout and a `HookDegraded`
    /// `SpanEvent` is recorded against the step's host span.
    async fn fire_pre_step(
        &self,
        session: &Session,
        job_id: JobId,
        step_kind: &StepKind,
        host: &mut StepHostSpan<'_>,
    ) -> std::result::Result<(), ()> {
        let Some(hooks) = self.hooks.as_ref() else {
            return Ok(());
        };
        let mut ctx = HookContext {
            session_id: session.id.to_string(),
            user_id: Some(session.user.id.clone()),
            event_data: HookEventData::PreStep {
                job_id: job_id.to_string(),
                step_kind: step_kind.tag().to_string(),
            },
            message: None,
            response: None,
            job_id: Some(job_id.to_string()),
            trace_span_id: None,
            extra: Default::default(),
        };
        match tokio::time::timeout(
            STEP_HOOK_TIMEOUT,
            hooks.trigger(HookPoint::PreStep, &mut ctx),
        )
        .await
        {
            Err(_elapsed) => {
                warn!(
                    timeout_ms = STEP_HOOK_TIMEOUT.as_millis() as u64,
                    "PreStep hook exceeded timeout, proceeding (default-allow)"
                );
                host.record_hook_degraded("pre_step_chain", HookPhase::PreStep)
                    .await;
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(error = %e, "PreStep hook chain failed, proceeding");
                Ok(())
            }
            Ok(Ok(HookAction::Abort(reason))) => {
                warn!(reason = %reason, "PreStep hook aborted step");
                Err(())
            }
            Ok(Ok(_)) => Ok(()),
        }
    }

    async fn fire_post_step(
        &self,
        session: &Session,
        job_id: JobId,
        step_id: aura_model::StepId,
        step_kind: &StepKind,
        outcome: &LifecycleOutcome,
        host: &mut StepHostSpan<'_>,
    ) {
        let Some(hooks) = self.hooks.as_ref() else {
            return;
        };
        let mut ctx = HookContext {
            session_id: session.id.to_string(),
            user_id: Some(session.user.id.clone()),
            event_data: HookEventData::PostStep {
                job_id: job_id.to_string(),
                step_id: step_id.to_string(),
                step_kind: step_kind.tag().to_string(),
                outcome: outcome.tag().to_string(),
            },
            message: None,
            response: None,
            job_id: Some(job_id.to_string()),
            trace_span_id: None,
            extra: Default::default(),
        };
        match tokio::time::timeout(
            STEP_HOOK_TIMEOUT,
            hooks.trigger(HookPoint::PostStep, &mut ctx),
        )
        .await
        {
            Err(_elapsed) => {
                warn!(
                    timeout_ms = STEP_HOOK_TIMEOUT.as_millis() as u64,
                    "PostStep hook exceeded timeout"
                );
                host.record_hook_degraded("post_step_chain", HookPhase::PostStep)
                    .await;
            }
            Ok(Err(e)) => warn!(error = %e, "PostStep hook chain failed"),
            Ok(Ok(_)) => {}
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
        if let Some(call) = stats.llm_call {
            // Post-hoc record — `SpanRecorder::end_span` publishes
            // `LlmSpanEnded` for the cost subscriber regardless of
            // wall-clock ordering.
            let step = span_recorder
                .begin_step(job_id, StepKind::Compression)
                .await?;
            let span = span_recorder
                .begin_span(
                    &step,
                    SpanKind::LlmCall {
                        model_id: call.model_id,
                        provider: call.provider,
                        provider_config_hash: String::new(),
                        input_messages: Vec::new(),
                        temperature: None,
                        output_content: String::new(),
                        thinking: None,
                        tool_calls: vec![],
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                    None,
                )
                .await?;
            span_recorder
                .end_span(
                    span,
                    job_id,
                    SpanResult::LlmCall {
                        output_content: String::new(),
                        thinking: None,
                        tool_calls: vec![],
                        input_tokens: call.input_tokens,
                        output_tokens: call.output_tokens,
                    },
                    LifecycleOutcome::Ok,
                )
                .await
                .ok();
            span_recorder
                .end_step(step, LifecycleOutcome::Ok)
                .await
                .ok();
        }
        Ok(())
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

    /// Run the LLM risk assessor against a selected skill. The caller
    /// surfaces `PassWithWarning` / `Block` verdicts as user-facing
    /// notices. All non-verdict outcomes — no assessor configured,
    /// inline skill with no on-disk source, or a transient assessor
    /// error — fall through as `Pass` so the agent stays functional
    /// when the judge is unavailable.
    async fn assess_skill_risk(&self, skill: &aura_skills::SkillDefinition) -> SkillGate {
        let Some(assessor) = self.skill_assessor.as_ref() else {
            return SkillGate::Pass;
        };
        match assessor.check(skill).await {
            Ok(assessed) => match assessed.verdict.level {
                RiskLevel::Dangerous => {
                    warn!(
                        skill = %skill.name,
                        scope = %assessed.scope.as_str(),
                        rationale = %assessed.verdict.rationale,
                        "skill blocked by risk assessor"
                    );
                    SkillGate::Block {
                        rationale: assessed.verdict.rationale,
                    }
                }
                RiskLevel::Suspicious => {
                    warn!(
                        skill = %skill.name,
                        scope = %assessed.scope.as_str(),
                        background_pending = assessed.background_pending,
                        rationale = %assessed.verdict.rationale,
                        "skill rated suspicious — injecting with warning"
                    );
                    SkillGate::PassWithWarning {
                        rationale: assessed.verdict.rationale,
                    }
                }
                RiskLevel::Safe => SkillGate::Pass,
            },
            Err(aura_skills_assessor::AssessError::NoSourcePath) => SkillGate::Pass,
            Err(err) => {
                warn!(
                    skill = %skill.name,
                    error = %err,
                    "risk assessor failed; allowing skill through"
                );
                SkillGate::Pass
            }
        }
    }

    /// Fire an `AgentOutput::Notice` telling the user that a skill they
    /// explicitly invoked was flagged by the risk assessor. `headline`
    /// is the short verb ("blocked", "rated suspicious"); `rationale`
    /// is the assessor's free-form reason. Silently no-ops when no
    /// output channel is attached (e.g. cron-triggered turns with no
    /// live user surface).
    async fn emit_skill_notice(
        &self,
        session: &Session,
        level: NoticeLevel,
        skill_name: &str,
        headline: &str,
        rationale: &str,
        output_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) {
        let Some(tx) = output_tx else { return };
        let text = format!("Skill '{skill_name}' {headline}: {rationale}");
        if tx
            .send(AgentOutput::Notice {
                session_id: session.id.to_string(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                level,
                text,
            })
            .await
            .is_err()
        {
            debug!("notice receiver dropped; skipping skill risk notice");
        }
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
