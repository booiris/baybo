use std::sync::Arc;

use aura_channels::{AgentOutput, NoticeLevel, OutgoingMessage};
use aura_context::ContextManager;
use aura_job::OperationKind;
use aura_llm::{
    ChatRequest, LlmClient, LlmResponse, StreamEvent, TokenUsage, ToolDefinitionForLlm,
};
use aura_model::{ChatMessage, ContentBlock, Role};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::memory::MemoryManager;
use aura_session::Session;
use aura_skills::SkillRegistry;
use aura_skills_assessor::{RiskLevel, SkillAssessor};
use aura_tools::ToolRegistry;
use aura_trace::{ExecutionProvenance, SpanInput, SpanResult, TraceNodeId};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::error_recovery::ErrorHandler;
use crate::observability::ObservabilityRecorder;
use crate::policy::ExecutionPolicy;
use crate::soul::Soul;
use crate::tool_executor::ToolExecutor;

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
    llm_client: Arc<LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    skill_registry: Arc<SkillRegistry>,
    tool_executor: Arc<ToolExecutor>,
    context_manager: ContextManager,
    memory_manager: Arc<MemoryManager>,
    policy: ExecutionPolicy,
    soul: Soul,
    error_handler: ErrorHandler,
    /// Optional LLM risk assessor. When set, every skill candidate is
    /// checked before injection: `Dangerous` verdicts veto the skill,
    /// `Suspicious` verdicts log a warning but allow it through.
    skill_assessor: Option<Arc<SkillAssessor>>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        llm_client: Arc<LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        skill_registry: Arc<SkillRegistry>,
        tool_executor: Arc<ToolExecutor>,
        context_manager: ContextManager,
        memory_manager: Arc<MemoryManager>,
        policy: ExecutionPolicy,
        soul: Soul,
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
            error_handler: ErrorHandler::default(),
            skill_assessor: None,
        }
    }

    pub fn with_skill_assessor(mut self, assessor: Arc<SkillAssessor>) -> Self {
        self.skill_assessor = Some(assessor);
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
        // Ensure system prompt is present
        self.ensure_system_prompt(session);

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
            self.context_manager.append(session, &memory_msg).await?;
        }

        // Append user message (auto-compresses if over token budget)
        let user_msg = ChatMessage {
            role: Role::User,
            content: user_content.clone(),
        };
        self.context_manager.append(session, &user_msg).await?;

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
            session.messages.push(ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(aura_skills::render::render_skill_block(
                    skill,
                ))],
            });
        }

        session.state.active_skills = active_skills.iter().map(|s| s.name.clone()).collect();

        // `allowed_tools` is the union of the active skills' declared
        // allow-lists. Union over intersection means the user doesn't
        // get starved when two active skills have disjoint needs; the
        // tradeoff is accepting the most permissive policy across
        // active skills, which matches the user's explicit intent to
        // invoke all of them together.
        let allowed_tools: Option<Vec<String>> = if active_skills.is_empty() {
            None
        } else {
            let mut union: Vec<String> = Vec::new();
            for s in &active_skills {
                for t in &s.allowed_tools {
                    if !union.iter().any(|existing| existing == t) {
                        union.push(t.clone());
                    }
                }
            }
            Some(union)
        };

        // Iterative LLM loop
        let mut iterations = 0;
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
                .call_llm_with_retry(
                    session,
                    recorder,
                    parent_job_id,
                    allowed_tools.as_deref(),
                    iter_delta_tx,
                )
                .await?;

            // Auto-snapshot after LLM call if the interval has been reached
            self.maybe_take_snapshot(session, recorder).await;

            // If no tool calls, we have the final response
            if response.tool_calls.is_empty() {
                // Use content_blocks when available, falling back to the text string.
                let final_blocks = if response.content_blocks.is_empty() {
                    vec![ContentBlock::Text(response.content.clone())]
                } else {
                    response.content_blocks.clone()
                };

                let final_text = aura_llm::multimodal::extract_text(&final_blocks);

                info!(
                    iterations,
                    content_len = final_text.len(),
                    "conversation loop complete"
                );

                // Append assistant response to context
                let assistant_msg = ChatMessage {
                    role: Role::Assistant,
                    content: final_blocks.clone(),
                };
                self.context_manager.append(session, &assistant_msg).await?;

                // Maybe store memory
                if let Err(e) = self.memory_manager.maybe_store(session, &final_text).await {
                    warn!(error = %e, "failed to auto-store memory");
                }

                return Ok(OutgoingMessage {
                    session_id: session.id.clone(),
                    channel: session.channel,
                    content: final_blocks,
                    reply_to: None,
                    metadata: Default::default(),
                });
            }

            // Append assistant message with tool call indicator
            let assistant_msg = ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text(response.content.clone())],
            };
            self.context_manager.append(session, &assistant_msg).await?;

            // Execute tool calls
            for tool_call in &response.tool_calls {
                debug!(
                    tool = %tool_call.name,
                    "executing tool call"
                );

                let tool_result = self
                    .tool_executor
                    .execute(
                        &tool_call.name,
                        tool_call.arguments.clone(),
                        &session.id,
                        &session.user,
                        recorder,
                        parent_job_id,
                    )
                    .await;

                let result_text = match &tool_result {
                    Ok(output) => format!("{output:?}"),
                    Err(e) => format!("Error: {e}"),
                };

                // Append tool result to context (auto-compresses if needed)
                let tool_msg = ChatMessage {
                    role: Role::Tool,
                    content: vec![ContentBlock::Text(result_text)],
                };
                self.context_manager.append(session, &tool_msg).await?;

                // Auto-snapshot after tool execution if the interval has been reached
                self.maybe_take_snapshot(session, recorder).await;
            }
        }

        // If we exhausted iterations, return what we have
        Ok(OutgoingMessage {
            session_id: session.id.clone(),
            channel: session.channel,
            content: vec![ContentBlock::Text(
                "I've reached the maximum number of processing steps. Please try again with a simpler request.".to_string(),
            )],
            reply_to: None,
            metadata: Default::default(),
        })
    }

    /// Call the LLM with retry on transient errors using `ErrorHandler`.
    async fn call_llm_with_retry(
        &self,
        session: &Session,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
        allowed_tools: Option<&[String]>,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<LlmResponse> {
        let mut attempt = 0u32;
        loop {
            match self
                .call_llm(session, recorder, parent_job_id, allowed_tools, delta_tx)
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
    ///
    /// When `allowed_tools` is `Some`, only tools whose names appear in the
    /// list are sent to the model. This is used by the skill system to
    /// restrict tool access according to the skill's `allowed_tools` field.
    async fn call_llm(
        &self,
        session: &Session,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
        allowed_tools: Option<&[String]>,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<LlmResponse> {
        let model_id = self.llm_client.model_id().to_string();

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

        let tool_defs: Vec<ToolDefinitionForLlm> = self
            .tool_registry
            .tool_definitions()
            .into_iter()
            .filter(|td| {
                // When a skill restricts tools, only include those in the allowlist.
                match allowed_tools {
                    Some(list) => list.iter().any(|name| name == &td.name),
                    None => true,
                }
            })
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

        let llm_result = match delta_tx {
            Some(tx) => self.chat_streaming(&request, session, tx).await,
            None => self.llm_client.chat(&request).await,
        };

        match llm_result {
            Ok(response) => {
                let output_preview = if response.content.len() > 200 {
                    let mut end = 200;
                    while !response.content.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &response.content[..end])
                } else {
                    response.content.clone()
                };

                let result = SpanResult::LlmResponse {
                    output_preview,
                    input_tokens: response.usage.input_tokens,
                    output_tokens: response.usage.output_tokens,
                    reasoning_redacted: response.thinking.is_some(),
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

                Ok(response)
            }
            Err(e) => {
                let error_msg = e.to_string();
                recorder.fail(handle, &error_msg).await?;
                Err(e.into())
            }
        }
    }

    /// Run a streaming chat request, forwarding each text chunk through
    /// `delta_tx` while accumulating the full response to return.
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

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::Text(chunk) => {
                    content.push_str(&chunk);
                    if delta_tx
                        .send(AgentOutput::Delta {
                            session_id: session.id.clone(),
                            channel: session.channel,
                            text: chunk,
                        })
                        .await
                        .is_err()
                    {
                        // Receiver gone; keep draining the LLM stream so the
                        // provider connection closes cleanly.
                        debug!("delta receiver dropped, continuing without forwarding");
                    }
                }
                StreamEvent::ToolCall(info) => tool_calls.push(info),
                StreamEvent::Reasoning(r) => thinking.push_str(&r),
                StreamEvent::Usage(u) => usage = u,
            }
        }

        let content_blocks = if content.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text(content.clone())]
        };

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

    /// If the trace collector's auto-snapshot interval has been reached,
    /// capture a context snapshot and attach it to the current active leaf node.
    async fn maybe_take_snapshot(&self, session: &Session, recorder: &ObservabilityRecorder) {
        if let Some(node_id) = recorder.maybe_snapshot().await {
            let snapshot = self.context_manager.snapshot(session);
            if let Err(e) = recorder.attach_snapshot(&node_id, snapshot).await {
                warn!(error = %e, "failed to attach auto context snapshot");
            } else {
                debug!(node_id = %node_id, "auto context snapshot attached");
            }
        }
    }

    fn ensure_system_prompt(&self, session: &mut Session) {
        let has_system = session
            .messages
            .first()
            .is_some_and(|m| m.role == Role::System);
        if !has_system {
            session.messages.insert(
                0,
                ChatMessage {
                    role: Role::System,
                    content: vec![ContentBlock::Text(self.soul.system_prompt().to_string())],
                },
            );
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
                channel: session.channel,
                level,
                text,
            })
            .await
            .is_err()
        {
            debug!("notice receiver dropped; skipping skill risk notice");
        }
    }

    /// Roll back the session to a previous trace node.
    ///
    /// Reads the snapshot from `ObservabilityRecorder`, forks the trace tree,
    /// and restores the session messages and context budget from the snapshot.
    pub async fn rollback(
        &mut self,
        session: &mut Session,
        recorder: &ObservabilityRecorder,
        target_node: TraceNodeId,
    ) -> anyhow::Result<()> {
        let snapshot = recorder.rollback_to(&target_node).await?;
        self.context_manager.restore(session, &snapshot)?;
        info!(
            target = %target_node,
            restored_messages = snapshot.messages.len(),
            restored_tokens = snapshot.token_count,
            "session rolled back"
        );
        Ok(())
    }
}
