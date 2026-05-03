use std::sync::Arc;

use aura_channels::{AgentOutput, NoticeLevel, OutgoingMessage};
use aura_context::ContextManager;
use aura_job::OperationKind;
use aura_llm::{
    ChatRequest, LlmCompletion, LlmResponse, StreamEvent, TokenUsage, ToolDefinitionForLlm,
};
use aura_model::{ChatMessage, ContentBlock, Role};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::memory::MemoryManager;
use aura_model::Session;
use aura_skills::SkillRegistry;
use aura_skills_assessor::{RiskLevel, SkillAssessor};
use aura_tools::mcp::{NoSidecarMcp, SidecarMcpProvider};
use aura_tools::{ToolOutput, ToolRegistry, preview_params};
use aura_trace::{ExecutionProvenance, SpanInput, SpanResult};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::error_recovery::ErrorHandler;
use crate::observability::ObservabilityRecorder;
use crate::policy::ExecutionPolicy;
use crate::security::SecurityGateway;
use crate::session_log::{
    LlmCallOutcome, LlmCallRecord, LlmRequestMeta, LlmResponseMeta, SessionLlmLogger,
};
use crate::soul::Soul;
use crate::tool_executor::ToolExecutor;

/// The maximum amount of text we'll hold in the streaming buffer waiting
/// for a placeholder to complete. If a chunk ends with an open `[{` but no
/// closing `}]` arrives within this many bytes, we flush anyway — no real
/// placeholder is this long, so holding further would be a DoS vector.
const STREAM_BUFFER_HIGH_WATER: usize = 128;

/// Hard timeout on sidecar MCP `tools/call` round-trips. Bounds how
/// long a hung rmcp peer (sidecar process stuck, network partition,
/// server-side deadlock) can pin the per-session actor.
///
/// **Contract with sidecar-hosted MCP tools**: this MUST be strictly
/// greater than the maximum self-timeout any sidecar tool advertises.
/// `feishu_ask_user` caps at 600s, so the agent waits 660s here —
/// the sidecar's own timer always fires first, returns a `timeout`
/// result, and removes its pending waiter. If the agent timed out
/// before the sidecar did, a late user reply could be consumed by
/// an orphan waiter on the sidecar side and silently dropped (Codex
/// review). The 60s buffer absorbs rmcp round-trip latency + clock
/// skew so the race is effectively impossible.
///
/// Pre-Phase-3.6 this was 30s — too short for interactive sidecar
/// tools and the source of the lost-reply bug. New tools that need
/// to wait longer than 600s need to either grow rmcp cancellation
/// support or bump this constant in lockstep.
const SIDECAR_MCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(660);

/// Cap on the byte length of `params_preview` / `result_preview` carried
/// in `AgentOutput::ToolCall*` telemetry. The agent's own context-side
/// `cap_tool_output` is megabytes — way too much to fan out per-frame
/// to every connected sidecar. Streaming sidecars only render a one-
/// line indicator, so a tighter bound is fine.
const TOOL_TELEMETRY_PREVIEW_LEN: usize = 256;

/// Truncate `text` to at most `max_len` bytes, appending `…` when the
/// content was clipped. Mirrors `aura_tools::preview_params`'s
/// behavior on plain strings; we don't go through `preview_params` for
/// results because they're already strings (not JSON values).
fn truncate_preview(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }
    let mut end = max_len;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

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
    /// Optional LLM risk assessor. When set, every skill candidate is
    /// checked before injection: `Dangerous` verdicts veto the skill,
    /// `Suspicious` verdicts log a warning but allow it through.
    skill_assessor: Option<Arc<SkillAssessor>>,
    /// Optional per-session JSONL logger for LLM calls. When set, every
    /// `call_llm` invocation appends a record (request, response or
    /// error, latency, model metadata) to
    /// `<state>/sessions/<session_id>.jsonl`.
    session_log: Option<Arc<SessionLlmLogger>>,
    /// Lazy per-session sidecar MCP provider. Defaults to a no-op so
    /// gateways without any `mcp_tunnel`-capable sidecar (and
    /// standalone TUI builds) need no extra wiring. The gateway
    /// substitutes its `SidecarMcpManager` here when a sidecar
    /// connects.
    sidecar_mcp: Arc<dyn SidecarMcpProvider>,
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
            skill_assessor: None,
            session_log: None,
            sidecar_mcp: Arc::new(NoSidecarMcp),
        }
    }

    pub fn with_skill_assessor(mut self, assessor: Arc<SkillAssessor>) -> Self {
        self.skill_assessor = Some(assessor);
        self
    }

    pub fn with_session_log(mut self, logger: Arc<SessionLlmLogger>) -> Self {
        self.session_log = Some(logger);
        self
    }

    pub fn with_sidecar_mcp(mut self, provider: Arc<dyn SidecarMcpProvider>) -> Self {
        self.sidecar_mcp = provider;
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
    pub async fn run(
        &mut self,
        session: &mut Session,
        user_content: Vec<ContentBlock>,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<OutgoingMessage> {
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

            // Proactive compression before building the ChatRequest
            if let Some(stats) = self.context_manager.maybe_compress(session).await? {
                debug!(
                    before = stats.before_tokens,
                    after = stats.after_tokens,
                    "compressed context before LLM call"
                );
            }

            // Call LLM with retry on transient errors. Deltas are only
            // streamed on the first iteration of the loop — subsequent
            // iterations are post-tool-call continuations that should be
            // rendered as a single block once complete.
            let iter_delta_tx = if iterations == 1 {
                delta_tx.as_ref()
            } else {
                None
            };
            let response = self
                .call_llm_with_retry(session, recorder, parent_job_id, iter_delta_tx)
                .await?;

            // If no tool calls, we have the final response
            if response.tool_calls.is_empty() {
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
                    session_id: session.id.clone(),
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

            for tool_call in &response.tool_calls {
                debug!(
                    tool = %tool_call.name,
                    "executing tool call"
                );

                if let Some(tx) = delta_tx.as_ref() {
                    let _ = tx
                        .send(AgentOutput::ToolCallStarted {
                            session_id: session.id.clone(),
                            user_id: session.user.id.clone(),
                            channel: session.channel.clone(),
                            call_id: tool_call.id.clone(),
                            tool: tool_call.name.clone(),
                            params_preview: preview_params(
                                &tool_call.arguments,
                                TOOL_TELEMETRY_PREVIEW_LEN,
                            ),
                            description: None,
                        })
                        .await;
                }
                let tool_call_started = std::time::Instant::now();

                // Sidecar MCP tools (e.g. `lark/feishu_get_chat_info`)
                // are not in the local `ToolRegistry` — they live
                // behind the channel sidecar's MCP server. Approval
                // gate + local sandbox don't apply (the call is a
                // remote query against an already-trusted tenant
                // API, not a local resource access), but we still
                // need the rest of the security pipeline:
                //   * `reveal_in_value` so placeholder secrets in
                //     params expand to plaintext for the remote API,
                //   * a hard timeout so a hung rmcp peer can't pin
                //     the actor,
                //   * `sanitize_tool_output` so a remote response
                //     containing a leaked secret gets minted into a
                //     placeholder before it reaches the LLM context
                //     or session log,
                //   * an observability span so the call shows up in
                //     traces alongside local tool executions.
                let claims = self.sidecar_mcp.claims_tool(session, &tool_call.name).await;
                let tool_result: anyhow::Result<ToolOutput> = if claims {
                    self.dispatch_sidecar_tool(
                        session,
                        &tool_call.name,
                        tool_call.arguments.clone(),
                        recorder,
                        parent_job_id,
                    )
                    .await
                } else {
                    self.tool_executor
                        .execute(
                            &tool_call.name,
                            tool_call.arguments.clone(),
                            &session.id,
                            &session.user,
                            &approved,
                            recorder,
                            parent_job_id,
                        )
                        .await
                };

                // Classify the outcome for telemetry before consuming
                // the result for the LLM context. Errors / denials feed
                // the `error` field on the Completed event so streaming
                // sidecars can switch the indicator's color/icon.
                //
                // Telemetry rides the channel wire and lands in the
                // sidecar's logs / cards — sanitize through the security
                // gateway before emit so a tool error containing a
                // bearer token, connection string, or revealed secret
                // placeholder does not leak. `Ok(ToolOutput::Error)`
                // already passed `sanitize_tool_output` inside the
                // executor; the raw `Err(e)` path did not.
                let telemetry_error: Option<String> = match &tool_result {
                    Ok(ToolOutput::Error(msg)) => Some(msg.clone()),
                    Err(e) => {
                        if let Some(aura_tools::ToolError::Denied { .. }) =
                            e.downcast_ref::<aura_tools::ToolError>()
                        {
                            Some("denied by user".to_string())
                        } else {
                            let raw = e.to_string();
                            Some(
                                self.security_gateway
                                    .sanitize_error(&raw)
                                    .await
                                    .unwrap_or(raw),
                            )
                        }
                    }
                    _ => None,
                };

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

                if let Some(tx) = delta_tx.as_ref() {
                    let result_preview = if telemetry_error.is_some() {
                        String::new()
                    } else {
                        truncate_preview(&raw_result_text, TOOL_TELEMETRY_PREVIEW_LEN)
                    };
                    let _ = tx
                        .send(AgentOutput::ToolCallCompleted {
                            session_id: session.id.clone(),
                            user_id: session.user.id.clone(),
                            channel: session.channel.clone(),
                            call_id: tool_call.id.clone(),
                            tool: tool_call.name.clone(),
                            result_preview,
                            error: telemetry_error,
                            duration_ms: u64::try_from(tool_call_started.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                        })
                        .await;
                }

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
        }

        // If we exhausted iterations, return what we have. Tail-append any
        // attachments the tools produced so the user still receives the
        // file even when the agent ran out of reasoning budget.
        let mut content = vec![ContentBlock::Text(
            "I've reached the maximum number of processing steps. Please try again with a simpler request.".to_string(),
        )];
        content.extend(std::mem::take(&mut accumulated_attachments));
        Ok(OutgoingMessage {
            session_id: session.id.clone(),
            user_id: session.user.id.clone(),
            channel: session.channel.clone(),
            content,
            reply_to: None,
            metadata: Default::default(),
        })
    }

    /// Dispatch a tool call the sidecar MCP provider has claimed.
    ///
    /// Mirrors the security pipeline `ToolExecutor::execute` applies
    /// to local tools — minus approval gate / sandbox / trust gate
    /// (sidecar tools are remote tenant-API queries, not local
    /// resource accesses):
    ///
    ///  * `reveal_in_value` on params so placeholder-form secrets
    ///    expand to plaintext before they ride the rmcp wire.
    ///  * Observability span via `recorder.begin/succeed/fail` so
    ///    sidecar tool calls show up in traces alongside local
    ///    tool executions, with proper elapsed-time accounting.
    ///  * Hard `SIDECAR_MCP_TIMEOUT` on the rmcp round-trip so a
    ///    hung peer can't pin the per-session actor.
    ///  * `sanitize_tool_output` on the result so any secret-looking
    ///    bytes the remote API returned get minted into placeholders
    ///    before they reach the LLM context, session log, or
    ///    telemetry preview.
    async fn dispatch_sidecar_tool(
        &self,
        session: &Session,
        tool_name: &str,
        params: Value,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
    ) -> anyhow::Result<ToolOutput> {
        // Begin the observability span first so denial / timeout
        // failures still appear in the trace.
        let handle = recorder
            .begin(
                &session.id,
                OperationKind::ToolExecution {
                    tool_name: tool_name.to_string(),
                },
                parent_job_id,
                ExecutionProvenance::default(),
                SpanInput::ToolExecution {
                    parameters: params.clone(),
                },
            )
            .await?;

        // Reveal placeholders so the remote API receives plaintext.
        // The pre-reveal `params` was captured in the span input
        // above; the trace surface keeps placeholder form.
        let mut params_revealed = params;
        if let Err(e) = self
            .security_gateway
            .reveal_in_value(&mut params_revealed)
            .await
        {
            recorder
                .fail(handle, &format!("reveal_in_value: {e}"))
                .await?;
            return Err(e.into());
        }

        let dispatch_fut =
            self.sidecar_mcp
                .execute_for_session(session, tool_name, params_revealed);

        let started = std::time::Instant::now();
        let dispatch_result = match tokio::time::timeout(SIDECAR_MCP_TIMEOUT, dispatch_fut).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                let msg = format!("sidecar mcp call timed out after {SIDECAR_MCP_TIMEOUT:?}");
                recorder.fail(handle, &msg).await?;
                return Ok(ToolOutput::Error(msg));
            }
        };
        let elapsed = started.elapsed();

        // claims_tool returned true, so dispatch returning None is
        // a race (sidecar disconnected mid-call). Surface as an
        // error rather than falling through to local — the tool
        // doesn't exist locally either.
        let mut output = match dispatch_result {
            Some(Ok(out)) => out,
            Some(Err(reason)) => ToolOutput::Error(reason),
            None => ToolOutput::Error(
                "sidecar mcp: tool was claimed at preflight but unavailable at dispatch \
                 (sidecar likely disconnected); retry on next turn"
                    .to_string(),
            ),
        };

        if let Err(e) = self
            .security_gateway
            .sanitize_tool_output(&mut output)
            .await
        {
            warn!(error = %e, tool = tool_name, "sidecar mcp: sanitize_tool_output failed");
        }

        let output_value = serde_json::to_value(&output).unwrap_or(Value::Null);
        let span_result = SpanResult::ToolResult {
            output: output_value.clone(),
            success: !matches!(output, ToolOutput::Error(_)),
            latency: elapsed,
        };
        recorder.succeed(handle, output_value, span_result).await?;

        Ok(output)
    }

    /// Call the LLM with retry on transient errors using `ErrorHandler`.
    async fn call_llm_with_retry(
        &self,
        session: &Session,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<LlmResponse> {
        let mut attempt = 0u32;
        loop {
            match self
                .call_llm(session, recorder, parent_job_id, delta_tx)
                .await
            {
                Ok(response) => return Ok(response),
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

    /// Call the LLM with the current session context.
    async fn call_llm(
        &self,
        session: &Session,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<LlmResponse> {
        let model_id = self.llm_client.model_info().id.clone();

        let handle = recorder
            .begin(
                &session.id,
                OperationKind::LlmCall {
                    model: model_id.clone(),
                },
                parent_job_id,
                ExecutionProvenance {
                    model_id: Some(model_id.clone()),
                    provider: Some(self.llm_client.model_info().provider.clone()),
                    ..Default::default()
                },
                SpanInput::LlmCall {
                    input_messages: session.messages.clone(),
                    temperature: None,
                },
            )
            .await?;

        let mut tool_defs: Vec<ToolDefinitionForLlm> = self
            .tool_registry
            .tool_definitions()
            .into_iter()
            .map(|td| ToolDefinitionForLlm {
                name: td.name,
                description: td.description,
                parameters_schema: td.parameters_schema,
            })
            .collect();
        // Sidecar MCP tools are session-scoped: discovered on demand
        // for the current session so multi-bot deployments don't leak
        // tool surfaces between tenants. Most builds run the no-op
        // provider and pay no cost.
        let sidecar_tools = self.sidecar_mcp.tool_definitions_for_session(session).await;
        tool_defs.extend(sidecar_tools.into_iter().map(|td| ToolDefinitionForLlm {
            name: td.name,
            description: td.description,
            parameters_schema: td.parameters_schema,
        }));

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
                // Defensive scrub of LLM output before it touches trace,
                // memory, or session history. The LLM never saw real
                // secrets (input is already sanitized), but it may echo
                // back placeholders or fabricate secret-looking strings.
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

                let result = SpanResult::LlmResponse {
                    output_content: response.content.clone(),
                    input_tokens: response.usage.input_tokens,
                    output_tokens: response.usage.output_tokens,
                    thinking: response.thinking.clone(),
                    tool_calls: trace_tool_calls,
                    latency: std::time::Duration::from_millis(0),
                };

                let pricing = &self.llm_client.model_info().pricing;
                let cost_usd = (response.usage.input_tokens as f64 / 1_000_000.0
                    * pricing.input_per_1m_tokens)
                    + (response.usage.output_tokens as f64 / 1_000_000.0
                        * pricing.output_per_1m_tokens);

                recorder
                    .succeed(
                        handle,
                        serde_json::to_value(&response).unwrap_or(Value::Null),
                        result,
                    )
                    .await?;

                recorder
                    .record_cost(
                        &session.user.id,
                        &session.id,
                        "",
                        "",
                        &model_id,
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        cost_usd,
                    )
                    .await;

                if let Err(e) = recorder.flush().await {
                    warn!(error = %e, "failed to flush trace after LLM call");
                }

                Ok(response)
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
                recorder.fail(handle, &error_msg).await?;
                if let Err(fe) = recorder.flush().await {
                    warn!(error = %fe, "failed to flush trace after LLM failure");
                }
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
                session_id: session.id.clone(),
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
