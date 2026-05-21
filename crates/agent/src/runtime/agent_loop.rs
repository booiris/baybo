use std::sync::Arc;

use aura_channels::{AgentOutput, COMPACT_COMMAND, OutgoingMessage};
use aura_context::ContextManager;
use aura_job::{JobInput, JobLifecycle, JobOutput};
use aura_llm::{
    ChatRequest, GuardedLlm, LlmResponse, StreamEvent, TokenUsage, ToolDefinitionForLlm,
};
use aura_model::{ChatMessage, ContentBlock, JobId, Role, SystemSpawnRequest};
use futures::StreamExt;
use tokio::sync::mpsc;

use aura_model::Session;
use aura_skills::{SKILL_INPUT_NAME_FIELD, SKILL_TOOL_NAME, SkillRegistry, SkillSummary};
use aura_tools::{ToolOutput, ToolRegistry};
use aura_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanRecorder, StepHandle, StepKind,
};
use tracing::{debug, info, warn};

use crate::runtime::compression::CompressionRunner;
use crate::runtime::error_recovery::ErrorHandler;
use crate::runtime::scope::JobSpec;
use crate::runtime::session_log::{
    LlmCallOutcome, LlmCallRecord, LlmRequestMeta, LlmResponseMeta, SessionLlmLogger,
};
use crate::runtime::soul::Soul;
use crate::runtime::tool_executor::ToolExecutor;
use crate::security::SecurityGateway;
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

/// Drop leading whitespace from `chunk` until the first non-whitespace
/// character has been observed across the stream.
///
/// `stripped` is the cross-chunk flag tracking "have we emitted any real
/// content yet?". While `false`, whitespace at the head of `chunk` is
/// removed; the first non-whitespace char flips the flag and every
/// subsequent call is a pass-through (interior whitespace, paragraph
/// breaks, code-block indentation are all preserved).
///
/// Returns `true` when the (possibly trimmed) chunk has content the
/// caller should emit, `false` when the chunk was pure whitespace and
/// should be skipped.
fn skip_leading_whitespace(chunk: &mut String, stripped: &mut bool) -> bool {
    if *stripped {
        return !chunk.is_empty();
    }
    let after = chunk.trim_start();
    if after.is_empty() {
        chunk.clear();
        return false;
    }
    if after.len() != chunk.len() {
        let kept = after.to_string();
        *chunk = kept;
    }
    *stripped = true;
    true
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
    session_id: aura_model::SessionId,
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
    /// LLM emitted tool calls; loop continues.
    Continue,
}

/// Core conversation loop: LLM call -> parse -> Tool/Skill dispatch -> repeat.
pub struct AgentLoop {
    llm_client: Arc<GuardedLlm>,
    /// Plumbed into [`crate::runtime::tool_executor::ToolExecutor::execute`] so
    /// in-tool LLM calls bill against the same model the surrounding
    /// actor is using.
    billed_chat_factory: Arc<crate::runtime::billed_chat::BilledChatFactory>,
    tool_registry: Arc<ToolRegistry>,
    skill_registry: Arc<SkillRegistry>,
    tool_executor: Arc<ToolExecutor>,
    context_manager: ContextManager,
    max_iterations: usize,
    soul: Soul,
    security_gateway: Arc<SecurityGateway>,
    error_handler: ErrorHandler,
    /// Cost gate + ledger; `record_call` feeds spend back so the
    /// `GuardedLlm` wrapper's gate sees it before the next dispatch.
    cost_manager: Arc<aura_cost::CostManager>,
    /// Lifetime token for the surrounding `AgentActor`. The spawner
    /// factory derives the token once and threads it into both this
    /// loop and the actor. The summary-refresh trigger gate clones it
    /// into outgoing `SystemSpawnRequest`s so cancellation cascades
    /// from the parent actor into its maintenance children
    /// automatically.
    actor_token: CancellationToken,
    /// Optional per-session JSONL logger for LLM calls. When set, every
    /// `call_llm` invocation appends a record (request, response or
    /// error, latency, model metadata) to
    /// `<workspace>/logs/sessions/<session_id>.jsonl`.
    session_log: Option<Arc<SessionLlmLogger>>,
    /// Sender half of the generic system-spawn channel. The router
    /// consumes the receiving half. Today only the summary-refresh
    /// trigger gate emits on it; future system tasks (history review,
    /// memory consolidation, ...) will share the same channel via
    /// other `SystemSpawnRequest` variants. `None` disables every
    /// system-trigger gate that gates on it.
    system_spawn_tx: Option<mpsc::Sender<SystemSpawnRequest>>,
    /// Resolved workspace paths. Today only the summary-refresh
    /// maintenance handler reads it (to write `summary.md`); other
    /// future system handlers may want it too. `None` in tests that
    /// don't exercise such handlers.
    workspace_paths: Option<Arc<aura_workspace::WorkspacePaths>>,
    /// Cross-session manager — used by handlers that operate across
    /// sessions (today: background summary, for transcript loads,
    /// in-flight maintenance lookups, summary metadata writes).
    /// Distinct from the `SessionManager` plumbed inside
    /// `ContextManager` because that one is per-session-bound.
    sessions: Option<Arc<crate::SessionManager>>,
}

/// Construction bundle for [`AgentLoop`]. Every field maps 1:1 to a
/// field on the loop; required deps are bare, optional deps are
/// `Option<T>`. Callers populate it via struct-literal syntax and pass
/// it to [`AgentLoop::from_config`] — no chained setters, no
/// post-construction mutability.
pub struct AgentLoopConfig {
    /// Process-wide pool of guarded LLM clients keyed by entry name.
    pub llm_pool: Arc<crate::runtime::llm_pool::LlmClientPool>,
    /// Initial pick for the active LLM. `None` ⇒ pool default.
    pub initial_llm: Option<String>,
    pub tool_registry: Arc<ToolRegistry>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_executor: Arc<ToolExecutor>,
    pub context_manager: ContextManager,
    pub max_iterations: usize,
    pub soul: Soul,
    pub security_gateway: Arc<SecurityGateway>,
    /// Cost gate + ledger.
    pub cost_manager: Arc<aura_cost::CostManager>,
    /// Lifetime token for the surrounding `AgentActor`. The spawner
    /// factory derives the actor token once and threads the same
    /// handle into both this loop and the actor.
    pub actor_token: CancellationToken,
    /// Optional per-session JSONL logger for LLM calls.
    pub session_log: Option<Arc<SessionLlmLogger>>,
    /// Generic system-spawn channel sender (any
    /// `SystemSpawnRequest` variant — today background summary).
    pub system_spawn_tx: Option<mpsc::Sender<SystemSpawnRequest>>,
    /// Workspace paths. Used by system handlers that touch on-disk
    /// state.
    pub workspace_paths: Option<Arc<aura_workspace::WorkspacePaths>>,
    /// Cross-session manager. Used by system handlers that operate
    /// across sessions.
    pub sessions: Option<Arc<crate::SessionManager>>,
}

impl AgentLoop {
    pub fn from_config(config: AgentLoopConfig) -> Self {
        let AgentLoopConfig {
            llm_pool,
            initial_llm,
            tool_registry,
            skill_registry,
            tool_executor,
            context_manager,
            max_iterations,
            soul,
            security_gateway,
            cost_manager,
            actor_token,
            session_log,
            system_spawn_tx,
            workspace_paths,
            sessions,
        } = config;
        let (llm_client, _effective_name) = llm_pool.resolve(initial_llm.as_deref());
        let billed_chat_factory = crate::runtime::billed_chat::BilledChatFactory::new(
            llm_client.clone(),
            cost_manager.clone(),
            Arc::clone(&security_gateway),
        );
        let mut context_manager = context_manager;
        context_manager.set_active_model_context_window(llm_client.model_info().context_window);

        Self {
            llm_client,
            billed_chat_factory,
            tool_registry,
            skill_registry,
            tool_executor,
            context_manager,
            max_iterations,
            soul,
            security_gateway,
            error_handler: ErrorHandler::default(),
            cost_manager,
            actor_token,
            session_log,
            system_spawn_tx,
            workspace_paths,
            sessions,
        }
    }

    /// Delegate to `ContextManager::restore_from_store` — the manager
    /// is bound to its session at construction time and owns the
    /// load path. Kept on `AgentLoop` so `AgentActor::run` doesn't
    /// have to reach inside the loop's private state.
    pub async fn restore_transcript_from_store(&mut self) {
        self.context_manager.restore_from_store().await;
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
        background_notice: Option<String>,
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
        crate::runtime::scope::with_job(
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
                        background_notice,
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
        background_notice: Option<String>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<OutgoingMessage> {
        let _ = job_lifecycle;
        self.ensure_system_prompt(session).await?;

        // Bound to the *outer* delta_tx, not iter_delta_tx — notices
        // need to reach the channel on iter-2+ where streaming is
        // suppressed.
        let notifier: Option<Arc<dyn aura_tools::SessionNotifier>> = delta_tx.as_ref().map(|tx| {
            Arc::new(DeltaTxNotifier {
                tx: tx.clone(),
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
            }) as Arc<dyn aura_tools::SessionNotifier>
        });

        // Background-subagent notice (work that finished off-thread since
        // the parent's last turn) is appended as its own context message
        // ahead of the user's — never merged into `user_content`, which
        // would push a leading `/command` past slash detection below.
        if let Some(notice) = background_notice {
            let notice_msg = ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text(notice)],
                from_user: false,
            };
            self.append_context_message(session, &notice_msg).await?;
        }

        let user_text = aura_llm::multimodal::extract_text(&user_content);
        let skills_for_turn = if self.skill_registry.is_empty() {
            Vec::new()
        } else {
            self.invocable_skills()
        };

        // Append user message (auto-compresses if over token budget).
        let user_msg = ChatMessage {
            role: Role::User,
            content: user_content.clone(),
            from_user: true,
        };
        self.append_context_message(session, &user_msg).await?;

        if let Some((skill_name, args)) = detect_slash_invocation(&user_text, &skills_for_turn) {
            let synthesized_id = format!("synthskill-{}", uuid::Uuid::new_v4());
            let mut input = serde_json::json!({ SKILL_INPUT_NAME_FIELD: skill_name });
            if !args.is_empty() {
                input["args"] = serde_json::Value::String(args);
            }
            let tool_use_block = ContentBlock::ToolUse {
                id: synthesized_id.clone(),
                name: SKILL_TOOL_NAME.to_string(),
                input: input.clone(),
                signature: None,
            };
            let assistant_msg = ChatMessage {
                role: Role::Assistant,
                content: vec![tool_use_block],
                from_user: false,
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
            let factory_clone = Arc::clone(&self.billed_chat_factory);

            let result_text = crate::runtime::scope::with_step(
                span_recorder.as_ref(),
                job_id,
                StepKind::LlmIteration,
                Some((&cancel_token, aura_job::CancelReason::ParentCancelled)),
                move |step| async move {
                    let res = executor_clone
                        .execute(
                            SKILL_TOOL_NAME,
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
                            Some(&factory_clone),
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
                from_user: false,
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
            if iterations >= self.max_iterations {
                warn!(max = self.max_iterations, "max iterations reached");
                break;
            }
            iterations += 1;

            // Iteration-boundary summary-refresh check. Skip iter 1 —
            // no work has happened yet to gate against.
            if iterations > 1 {
                self.maybe_spawn_background_compression(job_id, /* job_done */ false)
                    .await;
            }

            // Proactive compression before building the ChatRequest.
            self.compress_if_needed(session, span_recorder, job_id, &cancel_token)
                .await?;

            // Deltas are only streamed on the first iteration of the loop.
            let iter_delta_tx = if iterations == 1 {
                delta_tx.as_ref()
            } else {
                None
            };

            let outcome = crate::runtime::scope::with_step(
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
                IterationOutcome::Final(msg) => {
                    // End-of-job summary-refresh check. The activity
                    // disjunct is satisfied by `job_done = true`;
                    // the tokens / diff conjuncts still apply.
                    self.maybe_spawn_background_compression(job_id, /* job_done */ true)
                        .await;
                    return Ok(msg);
                }
                IterationOutcome::Continue => {
                    // Continue to the next LLM iteration; `run_iteration`
                    // has already appended each tool's result to the
                    // context.
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
        // Max-iterations fallback. No assistant row was persisted at
        // the loop end — the early-return path inside `run_iteration`
        // is the only one that calls `append_context_message`, so
        // there's no ordinal to stamp here.
        Ok(OutgoingMessage {
            session_id: session.id.clone(),
            user_id: session.user.id.clone(),
            channel: session.channel.clone(),
            content,
            reply_to: None,
            metadata: Default::default(),
            ordinal: None,
        })
    }

    /// One iteration of the agentic loop, scoped to a single
    /// `LlmIteration` step (opened by [`crate::runtime::scope::with_step`] in
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
            // prior iterations. Capture the persisted ordinal so the
            // channel adapter can stamp it onto the live `Frame::Message`
            // and reconnecting clients advance their cursor past it.
            let assistant_msg = ChatMessage {
                role: Role::Assistant,
                content: response_blocks,
                from_user: false,
            };
            let ordinal = self.append_context_message(session, &assistant_msg).await?;

            return Ok(IterationOutcome::Final(OutgoingMessage {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                content: final_blocks,
                reply_to: None,
                metadata: Default::default(),
                ordinal,
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
            from_user: false,
        };
        self.append_context_message(session, &assistant_msg).await?;

        // Execute tool calls concurrently. Approved resources are shared
        // via a Mutex so concurrent calls see each other's grants
        // immediately. Wrapped in an `Arc` so that any persist-always
        // closure injected into `ToolContext` mid-execution can clone
        // its handle into the executor boundary without a borrow
        // escape.
        //
        // Concurrency note: every tool call inside a single LLM
        // response runs in parallel via `futures::future::join_all`.
        // The post-execution pass that mutates `session.messages` is
        // kept SEQUENTIAL — appending tool results in original
        // `tool_calls` order keeps the next turn's context byte-stable
        // (prompt cache hits) and matches provider expectations that
        // tool_use ↔ tool_result pairs land in declaration order. The
        // executor + approval gate are already designed for
        // concurrency (TUI gate serialises its prompts internally).
        let approved = std::sync::Arc::new(parking_lot::Mutex::new(
            session.state.approved_resources.clone(),
        ));

        let executor = Arc::clone(&self.tool_executor);
        let session_id_for_calls = session.id.clone();
        let user_for_calls = session.user.clone();
        let recorder_for_calls = Arc::clone(span_recorder);
        let step_for_calls = step.clone();
        let notifier_for_calls = notifier.clone();
        let bcf_for_calls = Arc::clone(&self.billed_chat_factory);
        let exec_futures = response.tool_calls.iter().map(|tc| {
            let executor = Arc::clone(&executor);
            let session_id = session_id_for_calls.clone();
            let user = user_for_calls.clone();
            let approved = Arc::clone(&approved);
            let recorder = Arc::clone(&recorder_for_calls);
            let step = step_for_calls.clone();
            let cancel = cancel_token.child_token();
            let notifier = notifier_for_calls.clone();
            let bcf = Arc::clone(&bcf_for_calls);
            let tool_name = tc.name.clone();
            let arguments = tc.arguments.clone();
            let tool_use_id = tc.id.clone();
            let triggering_llm_span = Some(llm_span_id);
            async move {
                debug!(tool = %tool_name, "executing tool call");
                executor
                    .execute(
                        &tool_name,
                        arguments,
                        &session_id,
                        &user,
                        &approved,
                        &recorder,
                        &step,
                        triggering_llm_span,
                        tool_use_id,
                        None,
                        Some(job_id),
                        cancel,
                        notifier,
                        Some(&bcf),
                    )
                    .await
            }
        });
        let tool_results = futures::future::join_all(exec_futures).await;

        // Sequential post-processing: append results in `tool_calls`
        // order so context state stays byte-stable across calls.
        for (tool_call, tool_result) in response.tool_calls.iter().zip(tool_results) {
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
                from_user: false,
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
                    from_user: false,
                };
                self.append_context_message(session, &image_msg).await?;
            }
        }

        // Flush accumulated approvals back into session state.
        session.state.approved_resources = approved.lock().clone();

        Ok(IterationOutcome::Continue)
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

        // Coalesce adjacent same-role user/assistant messages *only* on the
        // wire to the LLM. Skill reminders are stored as standalone
        // `Role::User` entries, which would otherwise produce back-to-back
        // user messages — accepted by Anthropic / OpenAI but rejected by
        // providers that require strict user/assistant alternation.
        //
        // The trace span (and therefore the web UI / session log) keeps the
        // original unmerged transcript so each logical entry — reminder,
        // user prompt, assistant turn — stays separately inspectable.
        // Merging is a transport concern, not a storage one.
        let request = ChatRequest {
            messages: merge_for_llm(self.context_manager.messages()),
            temperature: None,
            tools: tool_defs,
        };

        let input_messages = self.context_manager.build_call_input_marker().await;

        crate::runtime::scope::with_llm_span(
            span_recorder.as_ref(),
            step,
            step.job_id,
            LlmCallBegin {
                model_id: model_info.id.clone(),
                provider: model_info.provider.clone(),
                provider_config_hash: String::new(),
                input_messages,
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
                        // Strip leading/trailing whitespace from text output
                        // before persisting. Some providers preface their
                        // first text block with stray newlines (especially
                        // after a thinking section or right after a tool
                        // call) which renders as a tall blank gap above the
                        // assistant message in the web UI. Interior
                        // whitespace is left intact so markdown paragraphs,
                        // code blocks, and lists are unaffected.
                        trim_response_text_edges(&mut response);
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
                        // Sanitize the JSONL log; return the raw typed
                        // `LlmError` so `should_retry` can dispatch on
                        // the variant. The trace's `Failed` reason
                        // still carries the provider text.
                        let raw = e.to_string();
                        let log_msg = self
                            .security_gateway
                            .sanitize_error(&raw)
                            .await
                            .unwrap_or(raw);
                        self.write_session_log(
                            session,
                            &request,
                            LlmCallOutcome::Err {
                                error: log_msg,
                                latency_ms,
                            },
                        )
                        .await;
                        // Bill the partial-stream tokens so a failed
                        // call still leaves a `cost_records` row.
                        let finalize = LlmCallResult {
                            output_content: String::new(),
                            thinking: None,
                            tool_calls: Vec::new(),
                            input_tokens: partial_usage.input_tokens,
                            output_tokens: partial_usage.output_tokens,
                            cached_input_tokens: partial_usage.cached_input_tokens,
                            cache_creation_input_tokens: partial_usage.cache_creation_input_tokens,
                        };
                        (finalize, Err(anyhow::Error::new(e)))
                    }
                };

                // Memory-first, disk-second: in-memory budget state
                // updates synchronously here so the next iteration's
                // `check()` sees this spend; persistence is fire-and-
                // forget inside record_call.
                self.cost_manager.record_call(
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

                // `record_call_actual` self-skips on zero. The
                // assistant message is appended later by
                // `append_context_message`, so the transcript at
                // this point still matches what the provider billed.
                self.context_manager
                    .record_call_actual(finalize.input_tokens);

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
            session_id: session.id.clone(),
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
        session: &Session,
        message: &ChatMessage,
    ) -> anyhow::Result<Option<i64>> {
        let ordinal = self.context_manager.append(message).await;
        self.write_session_message_log(session, message).await;
        Ok(ordinal)
    }

    async fn write_session_message_log(&self, session: &Session, message: &ChatMessage) {
        let Some(logger) = self.session_log.as_ref() else {
            return;
        };
        if let Err(e) = logger.log_message(&session.id, message).await {
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

        // Some providers preface their text with stray newlines (often
        // right after a thinking section or tool call). Drop them on the
        // wire so the user doesn't watch the message render with a tall
        // blank gap above. Once we've seen any non-whitespace char the
        // flag flips and subsequent chunks pass through verbatim — interior
        // formatting is preserved.
        let mut leading_stripped = false;

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
                        let mut flushable: String = pending.drain(..flush_to).collect();
                        if !skip_leading_whitespace(&mut flushable, &mut leading_stripped) {
                            continue;
                        }
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
            let mut flushable = std::mem::take(&mut pending);
            if skip_leading_whitespace(&mut flushable, &mut leading_stripped) {
                self.stream_emit(&flushable, session, &mut content, delta_tx)
                    .await;
            }
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
                session_id: session.id.clone(),
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

    /// Compress if the budget calls for it. The `chat` closure is
    /// invoked only when the strategy returns `NeedsLlmCall`; pure
    /// strategies (Truncate, Summarize fallback) skip it entirely. The
    /// closure brackets the real LLM call in a `Compression` step +
    /// `LlmCall` span and records cost against that span — budget
    /// enforcement on the call itself rides on the wrapped client.
    async fn compress_if_needed(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        cancel_token: &CancellationToken,
    ) -> anyhow::Result<()> {
        let runner = self.build_compression_runner(session, span_recorder, job_id, cancel_token);
        let model_id = runner.model_info.id.clone();
        let outcome = self
            .context_manager
            .maybe_compress(&model_id, |req| async move {
                runner.run(req).await.map(|run| run.response)
            })
            .await?;
        if matches!(outcome, aura_context::CompressionOutcome::Compressed) {
            self.reload_soul_after_compaction().await?;
        }
        Ok(())
    }

    fn build_compression_runner(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        cancel_token: &CancellationToken,
    ) -> CompressionRunner {
        let model_info = self.llm_client.model_info().clone();
        CompressionRunner {
            llm_client: self.llm_client.clone(),
            recorder: Arc::clone(span_recorder),
            cost_manager: self.cost_manager.clone(),
            security_gateway: Arc::clone(&self.security_gateway),
            job_id,
            user_id: session.user.id.clone(),
            session_id: session.id.clone(),
            model_info,
            cancel_token: cancel_token.clone(),
        }
    }

    /// Run an on-demand compression pass and return the confirmation
    /// text for the caller to ship as an `AgentOutput::Notice`.
    /// The variants of `CompressionOutcome` map to specific user-facing
    /// messages so the caller (typically a `/compact` notice) can
    /// distinguish "strategy declined", "no savings", and a real
    /// compress — instead of one generic "nothing to compress" line.
    /// A fresh job is minted so the compression step + LLM span land
    /// on a real lifecycle.
    pub async fn compact_now(
        &mut self,
        session: &mut Session,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        parent_job_id: Option<JobId>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<String> {
        // Match the session trigger so the JobKind invariant holds for
        // both user-triggered and spawned sessions. `/compact` is a
        // user-typed command, so UserChat is the natural default; for
        // sessions whose root trigger is anything else (Cron / System
        // / Spawned) we fall back to JobKind::Spawned which is allowed
        // under every trigger.
        let job_input = match session.trigger.kind() {
            aura_model::TriggerKind::User => JobInput::UserChat {
                content: vec![ContentBlock::Text(COMPACT_COMMAND.to_string())],
            },
            _ => JobInput::Spawned {
                initial_prompt: vec![ContentBlock::Text(COMPACT_COMMAND.to_string())],
            },
        };
        let spec = JobSpec {
            session_id: session.id.clone(),
            session_trigger_kind: session.trigger.kind(),
            input: job_input,
            effective_soul_version: session.bound_soul_version.clone(),
            parent_job_id,
        };

        crate::runtime::scope::with_job(
            job_lifecycle,
            cancel_token.clone(),
            spec,
            |job_id| async move {
                let runner =
                    self.build_compression_runner(session, span_recorder, job_id, &cancel_token);
                let model_id = runner.model_info.id.clone();
                let outcome = self
                    .context_manager
                    .force_compress(&model_id, |req| async move {
                        runner.run(req).await.map(|run| run.response)
                    })
                    .await?;
                if matches!(outcome, aura_context::CompressionOutcome::Compressed) {
                    self.reload_soul_after_compaction().await?;
                }
                let text = match outcome {
                    aura_context::CompressionOutcome::Compressed => {
                        "Context compressed.".to_string()
                    }
                    aura_context::CompressionOutcome::BelowThreshold => {
                        "Context already under the compression threshold; skipped.".to_string()
                    }
                    aura_context::CompressionOutcome::StrategyDeclined => {
                        "Compression strategy declined: nothing to summarize (conversation too short).".to_string()
                    }
                    aura_context::CompressionOutcome::NoSavings => {
                        "Compression ran but produced no savings; kept the original.".to_string()
                    }
                };
                let output = JobOutput::Message {
                    content: vec![ContentBlock::Text(text.clone())],
                };
                Ok((output, text))
            },
        )
        .await
    }

    /// Parent-side trigger gate. Fires at iteration boundaries and
    /// on terminal-state commit. When tokens and activity have
    /// crossed their thresholds and no maintenance session is
    /// already in flight for this parent, sends a
    /// `SystemSpawnRequest::BackgroundCompression` on the system-spawn
    /// channel for the router to materialise into a maintenance
    /// session + actor. Fire-and-forget — a full or closed channel
    /// never blocks the user's turn.
    ///
    /// `job_done = true` is passed at end-of-job (where the
    /// activity disjunct is trivially satisfied); `false` at
    /// iteration boundaries (where it relies on
    /// `tool_calls_since_anchor` exceeding the threshold).
    ///
    /// **Anchor-cursor sync (lazy pull).** The in-memory
    /// `last_summary_anchor` is only advanced by inline-compression
    /// applies. A successful background pass writes a fresh
    /// `session_summaries.cursor` but doesn't touch this loop's
    /// state — without intervention `tokens_since_anchor` would keep
    /// reporting the same delta and the gate would re-spawn a fresh
    /// pass on every later job. The fix is to read the metadata
    /// **once per evaluation** (we already need the same row for
    /// the `in_flight` check) and use its cursor to advance the
    /// in-memory anchor before measuring `tokens_since_anchor` /
    /// `tool_calls_since_anchor`. `sync_anchor_to_cursor` is
    /// monotonic, so a stale cursor or one already inside a
    /// rewritten slice is a no-op.
    async fn maybe_spawn_background_compression(
        &mut self,
        current_job_id: aura_model::JobId,
        job_done: bool,
    ) {
        let Some(tx) = self.system_spawn_tx.as_ref() else {
            return;
        };
        let tx = tx.clone();
        let actor_token = self.actor_token.clone();

        self.context_manager
            .maybe_request_background_summary(job_done, move |payload| {
                let request = SystemSpawnRequest::BackgroundCompression {
                    parent_session_id: payload.parent_session_id.clone(),
                    parent_job_id: current_job_id,
                    parent_actor_token: actor_token,
                    payload,
                };
                // `TrySendError<SystemSpawnRequest>` carries the
                // request back as its payload — large enough to trip
                // clippy's `result_large_err`. The gate only needs the
                // Display message for its rollback warn, so flatten
                // here.
                tx.try_send(request).map_err(|e| e.to_string())
            })
            .await;
    }

    /// Maintenance entry point — runs one async summary-refresh
    /// pass on behalf of the parent session named in `payload`. The
    /// surrounding actor (a System session with
    /// `LineageKind::SystemMaintenance`) invokes this from its
    /// `AgentMessage::SystemTrigger` mailbox handler, bypassing the
    /// normal chat-turn `run` cycle entirely. Wraps the LLM call in
    /// a `JobInput::System { reason: BackgroundCompression }` job so cost
    /// + trace are properly attributed.
    ///
    /// Errors when `workspace_paths` or `sessions` are unwired (test
    /// harnesses) — production bootstrap always sets both.
    pub(crate) async fn run_background_compression(
        &mut self,
        session: &mut Session,
        payload: aura_model::BackgroundCompressionPayload,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<aura_context::BackgroundSummaryOutcome> {
        let workspace_paths = self
            .workspace_paths
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("workspace_paths not configured for background summary")
            })?
            .clone();
        let sessions = self
            .sessions
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sessions not configured for background summary"))?
            .clone();
        // Held for the post-`with_job` defensive cleanup that
        // guarantees `in_flight` is cleared even when the runner
        // returns Err before reaching `record_summary_*` (cancel,
        // job-lifecycle rejection, transcript load failure). The
        // cleanup is gated on the owner token so a stale Pass A
        // finishing after Pass B already remarked the parent cannot
        // wipe Pass B's mark.
        let cleanup_sessions = sessions.clone();
        let parent_id_for_cleanup = payload.parent_session_id.clone();
        let cleanup_owner = payload.in_flight_owner.clone();
        let llm_client = self.llm_client.clone();
        let security_gateway = self.security_gateway.clone();
        let cost_manager = self.cost_manager.clone();
        let tokenizer = Arc::clone(self.context_manager.tokenizer());
        let model_info = self.llm_client.model_info().clone();
        let user_id = session.user.id.clone();
        let maintenance_session_id = session.id.clone();
        let recorder = span_recorder.clone();

        let spec = JobSpec {
            session_id: session.id.clone(),
            session_trigger_kind: session.trigger.kind(),
            input: aura_job::JobInput::System {
                payload: payload.clone(),
            },
            effective_soul_version: session.bound_soul_version.clone(),
            parent_job_id: session.lineage.as_ref().map(|l| l.parent_job_id),
        };

        let result = crate::runtime::scope::with_job(
            job_lifecycle,
            cancel_token.clone(),
            spec,
            move |job_id| {
                let payload = payload.clone();
                let cancel_token = cancel_token.clone();
                async move {
                    let refresher = crate::runtime::compression::BackgroundCompressionRunner {
                        llm_client,
                        security_gateway,
                        cost_manager,
                        sessions,
                        workspace_paths,
                        tokenizer,
                        recorder,
                        model_info,
                        maintenance_session_id,
                        maintenance_user_id: user_id,
                        job_id,
                        cancel_token,
                    };
                    let outcome = refresher.run(payload).await?;
                    let value = serde_json::to_value(&outcome)?;
                    let output = aura_job::JobOutput::Structured { value };
                    Ok((output, outcome))
                }
            },
        )
        .await;

        // Defense in depth: CAS-clear `in_flight` after the runner
        // returns. Successful and failed passes already clear the
        // flag via `record_summary_success`/`record_summary_failure`
        // (which also nulls `in_flight_owner`), so this owned-clear
        // is a no-op on those paths. It only fires for runner exits
        // that bypass `record_*` (cancellation before the runner
        // started, job-lifecycle rejection, mid-await drop). The
        // owner-token gate keeps a stale cleanup from this pass from
        // wiping a fresher pass' mark.
        match cleanup_sessions
            .clear_summary_in_flight_if_owned(&parent_id_for_cleanup, &cleanup_owner)
            .await
        {
            Ok(true) => {
                debug!(
                    parent_session_id = %parent_id_for_cleanup,
                    owner_token = %cleanup_owner,
                    "background summary: defensive in_flight clear fired (runner bypassed record_*)"
                );
            }
            Ok(false) => {
                // Either record_* already cleared it, or a fresher
                // pass took ownership — both are correct outcomes.
            }
            Err(e) => {
                warn!(
                    parent_session_id = %parent_id_for_cleanup,
                    error = %e,
                    "background summary: clear_summary_in_flight_if_owned after runner failed"
                );
            }
        }

        result
    }

    /// Seed the transcript with the soul system prompt and — once,
    /// adjacent to it — the authoritative skill reminder. Both are
    /// idempotent on the leading-system check: if `messages[0]` is
    /// already a system row, this is a no-op (restored sessions keep
    /// whatever they persisted; compression re-attaches the reminder
    /// via [`aura_context::insert_skill_trailer`] when the kept slice
    /// drops it).
    ///
    /// The reminder rides as `Role::User` because some providers reject
    /// `system` outside the leading slot; `merge_for_llm` folds it into
    /// the first real user message before dispatch. Placing it here
    /// (rather than per-turn after each user message) keeps the model's
    /// "what skills are available" context adjacent to its instructions
    /// and avoids appending a fresh reminder row every turn.
    async fn ensure_system_prompt(&mut self, session: &mut Session) -> anyhow::Result<()> {
        let skills = if self.skill_registry.is_empty() {
            Vec::new()
        } else {
            self.invocable_skills()
        };
        let soul_prompt = self.soul.system_prompt();
        let to_seed = initial_seed_messages(
            self.context_manager.messages().first(),
            soul_prompt,
            &skills,
        );
        for msg in &to_seed {
            self.append_context_message(session, msg).await?;
        }
        // active_skills mirrors the reminder we actually seeded; on the
        // re-entry path (leading system already present) `to_seed` is
        // empty and we leave the field unchanged so the prior value
        // carries forward.
        if !to_seed.is_empty() {
            session.state.active_skills = skills.iter().map(|s| s.name.clone()).collect();
        }
        Ok(())
    }

    /// Rebuild [`Soul`] from disk and swap the result into
    /// `messages[0]`. Called only after a successful compaction —
    /// the compressor preserves the leading system row from the
    /// pre-compaction transcript, so a profile edit made earlier in
    /// the conversation would otherwise carry the stale content
    /// forward forever. New sessions don't need this path because
    /// `ensure_system_prompt` already seeds them from a fresh
    /// [`Soul::from_workspace`] read.
    async fn reload_soul_after_compaction(&mut self) -> anyhow::Result<()> {
        let Some(paths) = self.workspace_paths.as_ref() else {
            return Ok(());
        };
        let workspace = aura_workspace::WorkspaceManager::new(paths.root().to_path_buf());
        let new_soul = match Soul::from_workspace(&workspace).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to reload soul from workspace; keeping cached prompt");
                return Ok(());
            }
        };
        // Only swap `messages[0]` when it's already a system text
        // block — defensive against an empty transcript or a non-
        // system first row.
        let first_is_system_text = self.context_manager.messages().first().is_some_and(|m| {
            m.role == Role::System && matches!(m.content.first(), Some(ContentBlock::Text(_)))
        });
        if first_is_system_text {
            self.context_manager.replace_first_message(ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(new_soul.system_prompt().to_string())],
                from_user: false,
            });
        }
        self.soul = new_soul;
        Ok(())
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

/// Pure decision logic for [`AgentLoop::ensure_system_prompt`]: given the
/// current leading message (if any), the resolved soul prompt, and the
/// invocable skill set, return the messages that should be appended.
///
/// Invariants:
/// - A leading `Role::System` message inhibits all seeding — empty vec.
/// - No leading system → exactly one system row is seeded.
/// - The skill reminder is appended **only** alongside a freshly-seeded
///   system row, never on the early-return path. This is what makes the
///   reminder fire exactly once per session: any subsequent call observes
///   the system row and short-circuits.
fn initial_seed_messages(
    leading: Option<&ChatMessage>,
    soul_prompt: &str,
    skills: &[SkillSummary],
) -> Vec<ChatMessage> {
    if leading.is_some_and(|m| m.role == Role::System) {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(if skills.is_empty() { 1 } else { 2 });
    out.push(ChatMessage {
        role: Role::System,
        content: vec![ContentBlock::Text(soul_prompt.to_string())],
        from_user: false,
    });
    if !skills.is_empty() {
        out.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(
                aura_skills::render::render_skill_reminder(skills),
            )],
            from_user: false,
        });
    }
    out
}

/// Strip leading/trailing whitespace from the LLM response's text fields.
///
/// `LlmResponse::content` is the flat aggregate string; `content_blocks`
/// preserves the structured form (text / thinking / tool_use / etc.). For
/// each text block — and for the aggregate string — leading/trailing
/// whitespace (including stray `\n` runs) is removed. Interior whitespace
/// is preserved so paragraph breaks, code blocks, and list formatting
/// inside the message stay intact.
///
/// Non-text blocks (`Thinking`, `ToolUse`, `ToolResult`, `Image`, etc.)
/// are untouched: their content is structured data the renderer / next
/// turn relies on verbatim.
fn trim_response_text_edges(response: &mut LlmResponse) {
    let trimmed = response.content.trim();
    if trimmed.len() != response.content.len() {
        response.content = trimmed.to_string();
    }
    for block in &mut response.content_blocks {
        if let ContentBlock::Text(t) = block {
            let trimmed = t.trim();
            if trimmed.len() != t.len() {
                *t = trimmed.to_string();
            }
        }
    }
}

/// Coalesce adjacent same-role user/assistant messages into a single
/// message so providers that require strict user/assistant alternation
/// (e.g. some Gemini / Mistral configurations) accept the request.
///
/// `Role::System` and `Role::Tool` are passed through untouched — system
/// messages are typically extracted to a dedicated field by the provider
/// adapter, and tool-result messages must remain individually addressable
/// by their `tool_use_id`.
///
/// When two adjacent user/assistant messages are merged, the merge also
/// flattens trailing/leading `ContentBlock::Text` blocks across the
/// boundary into a single text block (joined with `\n\n`). Non-text
/// blocks (images, tool_use, tool_result, thinking) are appended as-is so
/// signatures, IDs, and modality data are preserved verbatim.
fn merge_for_llm(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        let mergeable = matches!(msg.role, Role::User | Role::Assistant);
        match out.last_mut() {
            Some(last) if mergeable && last.role == msg.role => {
                for block in &msg.content {
                    let folded = matches!(block, ContentBlock::Text(_))
                        && matches!(last.content.last(), Some(ContentBlock::Text(_)));
                    if folded {
                        if let (Some(ContentBlock::Text(prev_t)), ContentBlock::Text(cur_t)) =
                            (last.content.last_mut(), block)
                        {
                            if !prev_t.is_empty() && !cur_t.is_empty() {
                                prev_t.push_str("\n\n");
                            }
                            prev_t.push_str(cur_t);
                        }
                    } else {
                        last.content.push(block.clone());
                    }
                }
            }
            _ => out.push(msg.clone()),
        }
    }
    out
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

#[cfg(test)]
mod skip_leading_whitespace_tests {
    use super::skip_leading_whitespace;

    #[test]
    fn pure_whitespace_first_chunk_is_skipped() {
        let mut chunk = String::from("\n\n\n");
        let mut stripped = false;
        let keep = skip_leading_whitespace(&mut chunk, &mut stripped);
        assert!(!keep);
        assert!(chunk.is_empty());
        assert!(!stripped, "no real content yet, flag must stay false");
    }

    #[test]
    fn mixed_first_chunk_strips_only_leading() {
        let mut chunk = String::from("\n\n  Hello\nworld");
        let mut stripped = false;
        let keep = skip_leading_whitespace(&mut chunk, &mut stripped);
        assert!(keep);
        assert_eq!(chunk, "Hello\nworld");
        assert!(stripped);
    }

    #[test]
    fn passthrough_after_first_real_content() {
        let mut stripped = true;
        let mut chunk = String::from("\n  next paragraph");
        let keep = skip_leading_whitespace(&mut chunk, &mut stripped);
        assert!(keep);
        assert_eq!(chunk, "\n  next paragraph", "interior whitespace preserved");
    }

    #[test]
    fn passthrough_empty_chunk_returns_false() {
        let mut stripped = true;
        let mut chunk = String::new();
        assert!(!skip_leading_whitespace(&mut chunk, &mut stripped));
    }

    #[test]
    fn sequential_whitespace_chunks_keep_flag_false() {
        let mut stripped = false;
        let mut a = String::from("  ");
        assert!(!skip_leading_whitespace(&mut a, &mut stripped));
        let mut b = String::from("\n");
        assert!(!skip_leading_whitespace(&mut b, &mut stripped));
        let mut c = String::from(" Hello");
        assert!(skip_leading_whitespace(&mut c, &mut stripped));
        assert_eq!(c, "Hello");
        assert!(stripped);
    }
}

#[cfg(test)]
mod trim_response_text_edges_tests {
    use super::trim_response_text_edges;
    use aura_llm::{LlmResponse, TokenUsage};
    use aura_model::ContentBlock;

    fn resp(content: &str, blocks: Vec<ContentBlock>) -> LlmResponse {
        LlmResponse {
            content: content.into(),
            content_blocks: blocks,
            tool_calls: Vec::new(),
            usage: TokenUsage::default(),
            thinking: None,
        }
    }

    #[test]
    fn strips_leading_newlines_on_aggregate_content() {
        let mut r = resp("\n\n\nHello world", Vec::new());
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, "Hello world");
    }

    #[test]
    fn strips_both_edges_keeps_interior() {
        let mut r = resp("\n  Title\n\nBody\n  ", Vec::new());
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, "Title\n\nBody");
    }

    #[test]
    fn strips_each_text_block() {
        let mut r = resp(
            "doesn't matter",
            vec![
                ContentBlock::Text("\n\nfirst paragraph".into()),
                ContentBlock::Text("second paragraph\n\n".into()),
            ],
        );
        trim_response_text_edges(&mut r);
        match &r.content_blocks[0] {
            ContentBlock::Text(t) => assert_eq!(t, "first paragraph"),
            other => panic!("expected text, got {other:?}"),
        }
        match &r.content_blocks[1] {
            ContentBlock::Text(t) => assert_eq!(t, "second paragraph"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn leaves_non_text_blocks_alone() {
        let tool_use = ContentBlock::ToolUse {
            id: "id1".into(),
            name: "Foo".into(),
            input: serde_json::json!({"k": "v"}),
            signature: None,
        };
        let mut r = resp("ignored", vec![tool_use.clone()]);
        trim_response_text_edges(&mut r);
        assert_eq!(r.content_blocks[0], tool_use);
    }

    #[test]
    fn no_op_when_already_clean() {
        let mut r = resp("Hello", vec![ContentBlock::Text("Already trimmed".into())]);
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, "Hello");
        match &r.content_blocks[0] {
            ContentBlock::Text(t) => assert_eq!(t, "Already trimmed"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn preserves_interior_code_block_indentation() {
        let body = "Heading\n\n    indented code line\n    second line";
        let mut r = resp(
            &format!("\n\n{body}\n\n"),
            vec![ContentBlock::Text(format!("\n{body}\n"))],
        );
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, body);
        match &r.content_blocks[0] {
            ContentBlock::Text(t) => assert_eq!(t, body),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod merge_for_llm_tests {
    use super::merge_for_llm;
    use aura_model::{BlobRef, ChatMessage, ContentBlock, Role};

    fn text(role: Role, body: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text(body.into())],
            from_user: false,
        }
    }

    fn img() -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:abc".into(),
            },
            mime_type: "image/png".into(),
        }
    }

    #[test]
    fn passthrough_when_alternating() {
        let msgs = vec![
            text(Role::System, "sys"),
            text(Role::User, "u1"),
            text(Role::Assistant, "a1"),
            text(Role::User, "u2"),
        ];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 4);
        assert_eq!(out, msgs);
    }

    #[test]
    fn merges_consecutive_user_text() {
        let msgs = vec![text(Role::User, "reminder"), text(Role::User, "hello")];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[0].content.len(), 1);
        match &out[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t, "reminder\n\nhello"),
            other => panic!("expected merged text, got {other:?}"),
        }
    }

    #[test]
    fn merges_consecutive_assistant_text() {
        let msgs = vec![text(Role::Assistant, "a"), text(Role::Assistant, "b")];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 1);
        match &out[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t, "a\n\nb"),
            other => panic!("expected merged text, got {other:?}"),
        }
    }

    #[test]
    fn keeps_non_text_blocks_separate() {
        let msgs = vec![
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text("hi".into()), img()],
                from_user: false,
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text("more".into())],
                from_user: false,
            },
        ];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 1);
        // hi, image, more — image keeps the text blocks from folding across it.
        assert_eq!(out[0].content.len(), 3);
        match &out[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t, "hi"),
            other => panic!("unexpected first block {other:?}"),
        }
        assert!(matches!(out[0].content[1], ContentBlock::Image { .. }));
        match &out[0].content[2] {
            ContentBlock::Text(t) => assert_eq!(t, "more"),
            other => panic!("unexpected third block {other:?}"),
        }
    }

    #[test]
    fn does_not_merge_system_or_tool() {
        let msgs = vec![
            text(Role::System, "s1"),
            text(Role::System, "s2"),
            ChatMessage {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "1".into(),
                    content: "r1".into(),
                }],
                from_user: false,
            },
            ChatMessage {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "2".into(),
                    content: "r2".into(),
                }],
                from_user: false,
            },
        ];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn preserves_assistant_tool_use_then_tool_result() {
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Foo".into(),
                input: serde_json::json!({}),
                signature: None,
            }],
            from_user: false,
        };
        let tool = ChatMessage {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
            }],
            from_user: false,
        };
        let msgs = vec![text(Role::User, "hi"), assistant.clone(), tool.clone()];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1], assistant);
        assert_eq!(out[2], tool);
    }
}

#[cfg(test)]
mod initial_seed_tests {
    use super::initial_seed_messages;
    use aura_model::{ChatMessage, ContentBlock, Role};
    use aura_skills::SkillSummary;

    const SOUL: &str = "You are Aura.";

    fn skill(name: &str) -> SkillSummary {
        SkillSummary {
            name: name.into(),
            command: None,
            description: format!("{name} description"),
            argument_hint: None,
            agent_invocable: true,
            trust_level: aura_model::TrustLevel::Trusted,
        }
    }

    fn system_row() -> ChatMessage {
        ChatMessage {
            role: Role::System,
            content: vec![ContentBlock::Text(SOUL.into())],
            from_user: false,
        }
    }

    fn user_row() -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("hi".into())],
            from_user: true,
        }
    }

    #[test]
    fn fresh_start_no_skills_seeds_system_only() {
        let out = initial_seed_messages(None, SOUL, &[]);
        assert_eq!(out.len(), 1, "expected one system row, got {out:?}");
        assert_eq!(out[0].role, Role::System);
        assert!(matches!(out[0].content[0], ContentBlock::Text(ref t) if t == SOUL));
    }

    #[test]
    fn fresh_start_with_skills_seeds_system_then_reminder() {
        let skills = vec![skill("alpha"), skill("beta")];
        let out = initial_seed_messages(None, SOUL, &skills);
        assert_eq!(out.len(), 2, "expected system + reminder, got {out:?}");
        assert_eq!(out[0].role, Role::System);
        assert_eq!(
            out[1].role,
            Role::User,
            "reminder rides as Role::User because some providers reject mid-stream system rows",
        );
        assert!(!out[1].from_user, "synthetic rows are never from_user");
        let reminder_text = match &out[1].content[0] {
            ContentBlock::Text(t) => t,
            _ => panic!("reminder should be a text block"),
        };
        assert!(reminder_text.contains("alpha"));
        assert!(reminder_text.contains("beta"));
    }

    #[test]
    fn leading_system_short_circuits_even_with_skills() {
        let leading = system_row();
        let skills = vec![skill("alpha")];
        let out = initial_seed_messages(Some(&leading), SOUL, &skills);
        assert!(
            out.is_empty(),
            "re-entry must not append anything; got {out:?}",
        );
    }

    #[test]
    fn leading_non_system_does_not_short_circuit() {
        // Defensive — a transcript whose first row is a user message
        // (test fixture, restored partial state) still gets seeded so
        // the LLM sees a leading system row.
        let leading = user_row();
        let out = initial_seed_messages(Some(&leading), SOUL, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::System);
    }

    #[test]
    fn second_call_after_first_seeds_nothing() {
        // The exactly-once invariant: simulate two consecutive calls by
        // feeding the previously-seeded system row as the leading
        // message on the second call.
        let first = initial_seed_messages(None, SOUL, &[skill("alpha")]);
        let leading = first[0].clone();
        let second = initial_seed_messages(Some(&leading), SOUL, &[skill("alpha")]);
        assert_eq!(first.len(), 2);
        assert!(
            second.is_empty(),
            "second call must short-circuit on the leading system row",
        );
    }
}
