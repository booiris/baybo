mod error;
pub mod multimodal;
mod providers;
pub mod registry;
mod think_tags;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{Stream, StreamExt};
use rig::OneOrMany;
use rig::completion::{
    self, AssistantContent, CompletionError, CompletionModel, CompletionRequest, GetTokenUsage,
    ToolDefinition,
};
use rig::message::{Message, Text, UserContent};
use rig::providers::{anthropic, gemini, openai};
use rig::streaming::{self, StreamedAssistantContent};
use serde::{Deserialize, Serialize};
use tracing::debug;

pub use crate::error::LlmError;
pub use crate::registry::{LlmProviderConfig, LlmProviderRegistry, ProviderModels};

pub type Result<T> = std::result::Result<T, LlmError>;

/// Metadata describing a model's capabilities and pricing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub context_window: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub pricing: ModelPricing,
}

/// Per-token pricing information for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_1m_tokens: f64,
    pub output_per_1m_tokens: f64,
}

/// Unified response structure returned by `LlmClient::chat()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    pub content_blocks: Vec<aura_model::ContentBlock>,
    pub tool_calls: Vec<ToolCallInfo>,
    pub usage: TokenUsage,
    pub thinking: Option<String>,
}

/// A single tool call extracted from the LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Provider-specific cryptographic signature (e.g. Gemini's
    /// `thought_signature`). Must be echoed back when the tool call is
    /// included in subsequent requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Token usage statistics for a single LLM call.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
}

/// A chat request to be sent to an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<aura_model::ChatMessage>,
    pub temperature: Option<f32>,
    pub tools: Vec<ToolDefinitionForLlm>,
}

/// A tool definition in the format expected by the LLM layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinitionForLlm {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

/// Events emitted during LLM streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text chunk from the model.
    Text(String),
    /// A complete tool call.
    ToolCall(ToolCallInfo),
    /// A reasoning/thinking text chunk (incremental delta).
    Reasoning(String),
    /// A complete, structured reasoning block. Must be preserved for
    /// providers that require thinking to be echoed back (Anthropic,
    /// Gemini).
    ThinkingBlock(aura_model::ContentBlock),
    /// Token usage statistics (emitted at stream end).
    Usage(TokenUsage),
}

/// A type-erased streaming response from an LLM provider.
pub struct LlmStream {
    inner: Pin<Box<dyn Stream<Item = crate::Result<StreamEvent>> + Send>>,
}

impl Stream for LlmStream {
    type Item = crate::Result<StreamEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl LlmStream {
    /// Build a stream from a fixed sequence of events. Gated to test
    /// builds only — production must construct streams via the rig
    /// adapter so provider-specific event mapping stays a single path.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_events(events: Vec<crate::Result<StreamEvent>>) -> Self {
        let stream = futures::stream::iter(events);
        Self {
            inner: Box::pin(stream),
        }
    }

    /// Wraps a rig `StreamingCompletionResponse` into our type-erased `LlmStream`,
    /// converting provider-specific events into `StreamEvent`.
    fn from_rig_stream<R>(rig_stream: streaming::StreamingCompletionResponse<R>) -> Self
    where
        R: Clone
            + Unpin
            + Send
            + Sync
            + GetTokenUsage
            + serde::Serialize
            + serde::de::DeserializeOwned
            + 'static,
    {
        let mapped = rig_stream.filter_map(|result| {
            futures::future::ready(match result {
                Err(e) => Some(Err(LlmError::Provider(e.to_string()))),
                Ok(event) => convert_stream_event(event),
            })
        });
        Self {
            inner: Box::pin(mapped),
        }
    }

    /// Wrap this stream so inline `<think>...</think>` content in
    /// `StreamEvent::Text` is rerouted through `Reasoning` /
    /// `ThinkingBlock` events. Used by providers (e.g. MiniMax M2)
    /// that bake reasoning into the regular text channel rather than
    /// emitting a separate `reasoning_content` field.
    fn with_inline_think_filter(self) -> Self {
        let filter = think_tags::ThinkTagFilter::default();
        let queue: VecDeque<crate::Result<StreamEvent>> = VecDeque::new();
        let stream =
            futures::stream::unfold((self, filter, queue), |(mut s, mut f, mut q)| async move {
                loop {
                    if let Some(event) = q.pop_front() {
                        return Some((event, (s, f, q)));
                    }
                    match s.next().await {
                        None => {
                            for ev in f.flush() {
                                q.push_back(Ok(ev));
                            }
                            if q.is_empty() {
                                return None;
                            }
                        }
                        Some(Err(e)) => return Some((Err(e), (s, f, q))),
                        Some(Ok(StreamEvent::Text(t))) => {
                            for ev in f.push_text(&t) {
                                q.push_back(Ok(ev));
                            }
                        }
                        Some(Ok(other)) => return Some((Ok(other), (s, f, q))),
                    }
                }
            });
        Self {
            inner: Box::pin(stream),
        }
    }
}

fn convert_stream_event<R: GetTokenUsage>(
    event: StreamedAssistantContent<R>,
) -> Option<crate::Result<StreamEvent>> {
    match event {
        StreamedAssistantContent::Text(t) => Some(Ok(StreamEvent::Text(t.text))),
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            Some(Ok(StreamEvent::ToolCall(ToolCallInfo {
                id: tool_call.id,
                name: tool_call.function.name,
                arguments: tool_call.function.arguments,
                signature: tool_call.signature,
            })))
        }
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            Some(Ok(StreamEvent::Reasoning(reasoning)))
        }
        StreamedAssistantContent::Reasoning(reasoning) => Some(Ok(StreamEvent::ThinkingBlock(
            convert_reasoning_to_block(&reasoning),
        ))),
        StreamedAssistantContent::Final(r) => r.token_usage().map(|usage| {
            Ok(StreamEvent::Usage(TokenUsage {
                input_tokens: usage.input_tokens as usize,
                output_tokens: usage.output_tokens as usize,
            }))
        }),
        // ToolCallDelta events are skipped — we emit the complete
        // ToolCall once it's fully assembled.
        _ => None,
    }
}

/// Enum-dispatched completion model supporting multiple providers.
pub(crate) enum AnyCompletionModel {
    OpenAI(openai::completion::CompletionModel),
    Anthropic(anthropic::completion::CompletionModel),
    Gemini(gemini::completion::CompletionModel),
}

impl AnyCompletionModel {
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<completion::CompletionResponse<()>, CompletionError> {
        match self {
            Self::OpenAI(m) => {
                let resp = m.completion(request).await?;
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage: resp.usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::Anthropic(m) => {
                let resp = m.completion(request).await?;
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage: resp.usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::Gemini(m) => {
                let resp = m.completion(request).await?;
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage: resp.usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<LlmStream, CompletionError> {
        match self {
            Self::OpenAI(m) => {
                let stream = m.stream(request).await?;
                Ok(LlmStream::from_rig_stream(stream))
            }
            Self::Anthropic(m) => {
                let stream = m.stream(request).await?;
                Ok(LlmStream::from_rig_stream(stream))
            }
            Self::Gemini(m) => {
                let stream = m.stream(request).await?;
                Ok(LlmStream::from_rig_stream(stream))
            }
        }
    }
}

/// Provider-agnostic completion contract used by the agent loop and any
/// other consumer that doesn't need the concrete `LlmClient` (e.g. probe).
///
/// Implemented by `LlmClient` for production and by test stubs in
/// `aura-llm`'s `test-support` feature for deterministic integration tests.
#[async_trait::async_trait]
pub trait LlmCompletion: Send + Sync {
    async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse>;
    async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream>;
    fn model_info(&self) -> &ModelInfo;
}

#[async_trait::async_trait]
impl LlmCompletion for LlmClient {
    async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        LlmClient::chat(self, request).await
    }
    async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream> {
        LlmClient::chat_stream(self, request).await
    }
    fn model_info(&self) -> &ModelInfo {
        LlmClient::model_info(self)
    }
}

/// The main LLM client type wrapping a rig completion model.
pub struct LlmClient {
    model_info: ModelInfo,
    model: AnyCompletionModel,
    /// Strip inline `<think>...</think>` content out of assistant text
    /// and route it through the reasoning channel. Set by providers
    /// (e.g. MiniMax M2) whose models bake CoT into the regular text
    /// stream instead of using a dedicated reasoning field.
    parse_inline_think_tags: bool,
}

impl LlmClient {
    /// Creates a new `LlmClient` from a provider-specific completion model.
    pub(crate) fn new(model_info: ModelInfo, model: AnyCompletionModel) -> Self {
        Self {
            model_info,
            model,
            parse_inline_think_tags: false,
        }
    }

    /// Opt in to stripping inline `<think>...</think>` blocks. See the
    /// field doc on `parse_inline_think_tags` for rationale.
    pub(crate) fn with_parse_inline_think_tags(mut self, enabled: bool) -> Self {
        self.parse_inline_think_tags = enabled;
        self
    }

    /// Sends a chat request to the provider and returns a unified response.
    pub async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        debug!(
            provider = %self.model_info.provider,
            model = %self.model_info.id,
            "sending chat request"
        );

        let rig_request = self.build_completion_request(request);

        let response = self
            .model
            .completion(rig_request)
            .await
            .map_err(|e| LlmError::Provider(e.to_string()))?;

        let llm_response = self.convert_response(response);

        debug!(
            content_len = llm_response.content.len(),
            tool_calls = llm_response.tool_calls.len(),
            input_tokens = llm_response.usage.input_tokens,
            output_tokens = llm_response.usage.output_tokens,
            "received LLM response"
        );

        Ok(llm_response)
    }

    /// Sends a chat request and returns a streaming response.
    pub async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream> {
        debug!(
            provider = %self.model_info.provider,
            model = %self.model_info.id,
            "sending streaming chat request"
        );

        let rig_request = self.build_completion_request(request);

        let stream = self
            .model
            .stream(rig_request)
            .await
            .map_err(|e| LlmError::Provider(e.to_string()))?;

        Ok(if self.parse_inline_think_tags {
            stream.with_inline_think_filter()
        } else {
            stream
        })
    }

    /// Build a rig `CompletionRequest` from our `ChatRequest`.
    fn build_completion_request(&self, request: &ChatRequest) -> CompletionRequest {
        let mut system_parts = Vec::new();
        let mut chat_messages: Vec<Message> = Vec::new();

        for msg in &request.messages {
            match msg.role {
                aura_model::Role::System => {
                    system_parts.push(multimodal::extract_text(&msg.content));
                }
                aura_model::Role::User => {
                    let mut parts: Vec<UserContent> = msg
                        .content
                        .iter()
                        .map(|block| match block {
                            aura_model::ContentBlock::Text(t) => {
                                UserContent::Text(Text { text: t.clone() })
                            }
                            other => UserContent::Text(Text {
                                text: multimodal::content_block_to_text(other),
                            }),
                        })
                        .collect();
                    if !parts.is_empty() {
                        let first = parts.remove(0);
                        let mut content = OneOrMany::one(first);
                        for part in parts {
                            content.push(part);
                        }
                        chat_messages.push(Message::User { content });
                    }
                }
                aura_model::Role::Assistant => {
                    let mut parts: Vec<AssistantContent> = Vec::new();
                    for block in &msg.content {
                        match block {
                            aura_model::ContentBlock::Text(t) if !t.is_empty() => {
                                parts.push(AssistantContent::Text(Text { text: t.clone() }));
                            }
                            aura_model::ContentBlock::ToolUse {
                                id,
                                name,
                                input,
                                signature,
                            } => {
                                parts.push(AssistantContent::ToolCall(
                                    completion::message::ToolCall {
                                        id: id.clone(),
                                        call_id: None,
                                        function: completion::message::ToolFunction {
                                            name: name.clone(),
                                            arguments: input.clone(),
                                        },
                                        signature: signature.clone(),
                                        additional_params: None,
                                    },
                                ));
                            }
                            aura_model::ContentBlock::Thinking { id, content } => {
                                parts.push(convert_thinking_to_reasoning(id, content));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        let first = parts.remove(0);
                        let mut content = OneOrMany::one(first);
                        for part in parts {
                            content.push(part);
                        }
                        chat_messages.push(Message::Assistant { id: None, content });
                    }
                }
                aura_model::Role::Tool => {
                    let mut parts: Vec<UserContent> = Vec::new();
                    for block in &msg.content {
                        match block {
                            aura_model::ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                            } => {
                                parts.push(UserContent::ToolResult(
                                    completion::message::ToolResult {
                                        id: tool_use_id.clone(),
                                        call_id: None,
                                        content: OneOrMany::one(
                                            completion::message::ToolResultContent::Text(Text {
                                                text: content.clone(),
                                            }),
                                        ),
                                    },
                                ));
                            }
                            aura_model::ContentBlock::Text(text) => {
                                // Legacy fallback for plain-text tool results.
                                parts.push(UserContent::Text(Text { text: text.clone() }));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        let first = parts.remove(0);
                        let mut content = OneOrMany::one(first);
                        for part in parts {
                            content.push(part);
                        }
                        chat_messages.push(Message::User { content });
                    }
                }
            }
        }

        let tools: Vec<ToolDefinition> = request
            .tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters_schema.clone(),
            })
            .collect();

        let preamble = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n"))
        };

        // Ensure at least one message for OneOrMany.
        if chat_messages.is_empty() {
            chat_messages.push(Message::User {
                content: OneOrMany::one(UserContent::Text(Text {
                    text: String::new(),
                })),
            });
        }

        let first = chat_messages.remove(0);
        let mut chat_history = OneOrMany::one(first);
        for msg in chat_messages {
            chat_history.push(msg);
        }

        CompletionRequest {
            model: None,
            preamble,
            chat_history,
            documents: Vec::new(),
            tools,
            temperature: request.temperature.map(|t| t as f64),
            max_tokens: Some(4096),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        }
    }

    /// Convert a rig `CompletionResponse` into our `LlmResponse`.
    fn convert_response(&self, response: completion::CompletionResponse<()>) -> LlmResponse {
        let mut content = String::new();
        let mut content_blocks = Vec::new();
        let mut tool_calls = Vec::new();
        let mut thinking: Option<String> = None;
        let mut inline_thinking = String::new();

        for item in response.choice.into_iter() {
            match item {
                AssistantContent::Text(text) => {
                    let (clean, extracted) = if self.parse_inline_think_tags {
                        think_tags::extract_inline_thinking(&text.text)
                    } else {
                        (text.text.clone(), None)
                    };
                    if let Some(t) = extracted {
                        if !inline_thinking.is_empty() {
                            inline_thinking.push('\n');
                        }
                        inline_thinking.push_str(&t);
                    }
                    if !clean.is_empty() {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&clean);
                        content_blocks.push(aura_model::ContentBlock::Text(clean));
                    }
                }
                AssistantContent::ToolCall(tc) => {
                    tool_calls.push(ToolCallInfo {
                        id: tc.id,
                        name: tc.function.name,
                        arguments: tc.function.arguments,
                        signature: tc.signature,
                    });
                }
                AssistantContent::Reasoning(ref r) => {
                    let reasoning_text: String = r
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            rig::completion::message::ReasoningContent::Text { text, .. } => {
                                Some(text.as_str())
                            }
                            rig::completion::message::ReasoningContent::Summary(s) => {
                                Some(s.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !reasoning_text.is_empty() {
                        thinking = Some(reasoning_text);
                    }
                    // Also preserve the full structured block for round-trip.
                    content_blocks.push(convert_reasoning_to_block(r));
                }
                AssistantContent::Image(_) => {}
            }
        }

        if !inline_thinking.is_empty() {
            thinking = Some(match thinking.take() {
                Some(existing) => format!("{existing}\n{inline_thinking}"),
                None => inline_thinking.clone(),
            });
            content_blocks.push(aura_model::ContentBlock::Thinking {
                id: None,
                content: vec![aura_model::ThinkingContent::Text {
                    text: inline_thinking,
                    signature: None,
                }],
            });
        }

        let usage = TokenUsage {
            input_tokens: response.usage.input_tokens as usize,
            output_tokens: response.usage.output_tokens as usize,
        };

        LlmResponse {
            content,
            content_blocks,
            tool_calls,
            usage,
            thinking,
        }
    }

    /// Returns the model identifier (e.g. `"claude-sonnet-4-6"`).
    pub fn model_id(&self) -> &str {
        &self.model_info.id
    }

    /// Returns the full model metadata.
    pub fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }

    /// Issue a minimal chat request to verify provider connectivity and auth.
    ///
    /// Used by `aura llm probe` and `aura doctor`. The request is deliberately
    /// tiny (one-token prompt, no tools) so it is cheap to run repeatedly.
    pub async fn probe(&self) -> crate::Result<ProbeReport> {
        let req = ChatRequest {
            messages: vec![aura_model::ChatMessage {
                role: aura_model::Role::User,
                content: vec![aura_model::ContentBlock::Text("ping".to_string())],
            }],
            temperature: Some(0.0),
            tools: vec![],
        };
        let start = std::time::Instant::now();
        let response = self.chat(&req).await?;
        Ok(ProbeReport {
            provider: self.model_info.provider.clone(),
            model: self.model_info.id.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
            tokens: response.usage,
        })
    }
}

/// Result of a successful `LlmClient::probe()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    pub provider: String,
    pub model: String,
    pub latency_ms: u64,
    pub tokens: TokenUsage,
}

// ---------------------------------------------------------------------------
// Reasoning / Thinking round-trip helpers
// ---------------------------------------------------------------------------

/// Convert a rig `Reasoning` block into our provider-agnostic
/// `ContentBlock::Thinking` representation.
fn convert_reasoning_to_block(
    reasoning: &completion::message::Reasoning,
) -> aura_model::ContentBlock {
    use aura_model::ThinkingContent;
    let content = reasoning
        .content
        .iter()
        .map(|c| match c {
            rig::completion::message::ReasoningContent::Text { text, signature } => {
                ThinkingContent::Text {
                    text: text.clone(),
                    signature: signature.clone(),
                }
            }
            rig::completion::message::ReasoningContent::Summary(s) => {
                ThinkingContent::Summary { text: s.clone() }
            }
            rig::completion::message::ReasoningContent::Encrypted(d)
            | rig::completion::message::ReasoningContent::Redacted { data: d } => {
                ThinkingContent::Redacted { data: d.clone() }
            }
            _ => ThinkingContent::Summary {
                text: String::new(),
            },
        })
        .collect();
    aura_model::ContentBlock::Thinking {
        id: reasoning.id.clone(),
        content,
    }
}

/// Convert our `ContentBlock::Thinking` back into a rig
/// `AssistantContent::Reasoning` for the completion request.
fn convert_thinking_to_reasoning(
    id: &Option<String>,
    content: &[aura_model::ThinkingContent],
) -> AssistantContent {
    use rig::completion::message::{Reasoning, ReasoningContent};
    let blocks = content
        .iter()
        .map(|tc| match tc {
            aura_model::ThinkingContent::Text { text, signature } => ReasoningContent::Text {
                text: text.clone(),
                signature: signature.clone(),
            },
            aura_model::ThinkingContent::Summary { text } => {
                ReasoningContent::Summary(text.clone())
            }
            aura_model::ThinkingContent::Redacted { data } => {
                ReasoningContent::Redacted { data: data.clone() }
            }
        })
        .collect();
    let mut reasoning = Reasoning::new("").optional_id(id.clone());
    reasoning.content = blocks;
    AssistantContent::Reasoning(reasoning)
}
