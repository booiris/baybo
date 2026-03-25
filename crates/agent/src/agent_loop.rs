use std::sync::Arc;

use aura_context::ContextManager;
use aura_core::{ChatMessage, ContentBlock, OperationKind, OutgoingMessage, Role, Session};
use aura_llm::{ChatRequest, LlmClient, LlmResponse, ToolDefinitionForLlm};
use aura_memory::MemoryManager;
use aura_tools::ToolRegistry;
use aura_trace::{ExecutionProvenance, SpanInput, SpanResult, TraceNodeId};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::observability::ObservabilityRecorder;
use crate::policy::ExecutionPolicy;
use crate::soul::Soul;
use crate::tool_executor::ToolExecutor;

/// Core conversation loop: LLM call -> parse -> Tool/Skill dispatch -> repeat.
pub struct AgentLoop {
    llm_client: Arc<LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    tool_executor: Arc<ToolExecutor>,
    context_manager: Box<dyn ContextManager>,
    memory_manager: Arc<MemoryManager>,
    policy: ExecutionPolicy,
    soul: Soul,
}

impl AgentLoop {
    pub fn new(
        llm_client: Arc<LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        tool_executor: Arc<ToolExecutor>,
        context_manager: Box<dyn ContextManager>,
        memory_manager: Arc<MemoryManager>,
        policy: ExecutionPolicy,
        soul: Soul,
    ) -> Self {
        Self {
            llm_client,
            tool_registry,
            tool_executor,
            context_manager,
            memory_manager,
            policy,
            soul,
        }
    }

    /// Run the main conversation loop for a single user message.
    pub async fn run(
        &mut self,
        session: &mut Session,
        user_content: Vec<ContentBlock>,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
    ) -> aura_core::Result<OutgoingMessage> {
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
            self.context_manager
                .append(session, Role::System, &memory_msg)
                .await?;
        }

        // Append user message
        let user_msg = ChatMessage {
            role: Role::User,
            content: user_content,
        };
        self.context_manager
            .append(session, Role::User, &user_msg)
            .await?;

        // Maybe compress context
        let compress_result = self.context_manager.maybe_compress(session).await?;
        if compress_result.compressed {
            debug!(
                before = compress_result.before_tokens,
                after = compress_result.after_tokens,
                "context compressed"
            );
        }

        // Iterative LLM loop
        let mut iterations = 0;
        loop {
            if iterations >= self.policy.max_iterations {
                warn!(max = self.policy.max_iterations, "max iterations reached");
                break;
            }
            iterations += 1;

            // Call LLM
            let response = self.call_llm(session, recorder, parent_job_id).await?;

            // Auto-snapshot after LLM call if the interval has been reached
            self.maybe_take_snapshot(session, recorder).await;

            // If no tool calls, we have the final response
            if response.tool_calls.is_empty() {
                let final_content = response.content.clone();
                info!(
                    iterations,
                    content_len = final_content.len(),
                    "conversation loop complete"
                );

                // Append assistant response to context
                let assistant_msg = ChatMessage {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text(final_content.clone())],
                };
                self.context_manager
                    .append(session, Role::Assistant, &assistant_msg)
                    .await?;

                // Maybe store memory
                if let Err(e) = self
                    .memory_manager
                    .maybe_store(session, &final_content)
                    .await
                {
                    warn!(error = %e, "failed to auto-store memory");
                }

                return Ok(OutgoingMessage {
                    session_id: session.id.clone(),
                    channel: session.channel,
                    content: vec![ContentBlock::Text(final_content)],
                    reply_to: None,
                    metadata: Default::default(),
                });
            }

            // Append assistant message with tool call indicator
            let assistant_msg = ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text(response.content.clone())],
            };
            self.context_manager
                .append(session, Role::Assistant, &assistant_msg)
                .await?;

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

                // Append tool result to context
                let tool_msg = ChatMessage {
                    role: Role::Tool,
                    content: vec![ContentBlock::Text(result_text)],
                };
                self.context_manager
                    .append(session, Role::Tool, &tool_msg)
                    .await?;

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

    /// Call the LLM with the current session context.
    async fn call_llm(
        &self,
        session: &Session,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
    ) -> aura_core::Result<LlmResponse> {
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

        match self.llm_client.chat(&request).await {
            Ok(response) => {
                let output_preview = if response.content.len() > 200 {
                    format!("{}...", &response.content[..200])
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
                Err(e)
            }
        }
    }

    /// Roll back the conversation to a previous trace node.
    ///
    /// This forks the trace tree from `node_id`, retrieves the nearest context
    /// snapshot, and restores the session state to that point. Returns the
    /// fork id for the newly created branch.
    pub async fn rollback(
        &self,
        session: &mut Session,
        node_id: TraceNodeId,
        recorder: &ObservabilityRecorder,
    ) -> aura_core::Result<String> {
        let (fork_id, snapshot) = recorder.rollback(node_id).await?;
        self.context_manager.restore_state(session, &snapshot)?;
        Ok(fork_id)
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
}
