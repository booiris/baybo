pub mod billed;
pub mod credentials;
mod error;
pub mod guard;
pub mod multimodal;
pub mod openrouter;
pub mod providers;
pub mod registry;
mod tool_name;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{Stream, StreamExt};
use rig::OneOrMany;
use rig::completion::message::{
    Audio, AudioMediaType, Document, DocumentMediaType, DocumentSourceKind, Image, ImageDetail,
    ImageMediaType,
};
use rig::completion::{
    self, AssistantContent, CompletionError, CompletionModel, CompletionRequest, GetTokenUsage,
    ToolDefinition,
};
use rig::message::{Message, Text, UserContent};
use rig::providers::{
    anthropic, cohere, deepseek, gemini, groq, huggingface, hyperbolic, llamafile, minimax,
    mistral, moonshot, ollama, openai, perplexity, together, xai, xiaomimimo, zai,
};
use rig::streaming::{self, StreamedAssistantContent};
use serde::{Deserialize, Serialize};
use tracing::debug;

pub use crate::billed::{
    Attribution, BilledChat, BilledChatResponse, BoundBilledLlm, CostHooks, LlmCostRecorder,
    SYSTEM_USER_ID,
};
pub use crate::error::LlmError;
pub(crate) use crate::error::{
    proxied_client, reqwest_to_error, rig_completion_to_error, status_to_error,
};
pub use crate::guard::{BillableLlm, LlmCallGuard};
pub use crate::providers::{FactoryDefaults, factory_defaults_for};
pub use crate::registry::{
    LiveModelInfo, LlmPricingOverride, LlmProviderConfig, LlmProviderRegistry,
};

/// Default chat-completion base URL for each built-in provider id.
/// `None` for unknown providers — the operator must supply one.
pub fn default_base_url_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some(crate::providers::openai::OPENAI_DEFAULT_BASE_URL),
        "anthropic" => Some(crate::providers::anthropic::ANTHROPIC_DEFAULT_BASE_URL),
        "gemini" => Some(crate::providers::gemini::GEMINI_DEFAULT_BASE_URL),
        "minimax" => Some(crate::providers::minimax::MINIMAX_DEFAULT_BASE_URL),
        "deepseek" => Some(crate::providers::deepseek::DEEPSEEK_DEFAULT_BASE_URL),
        crate::providers::openai_subscription::PROVIDER_NAME => {
            Some(crate::providers::openai_subscription::DEFAULT_BASE_URL)
        }
        _ => None,
    }
}

/// Strip `; charset=…` and lowercase, then map common MIME strings to
/// rig's `ImageMediaType` enum. `None` means the model can't natively
/// consume this image kind — the caller falls back to a text stub.
fn parse_image_media_type(mime: &str) -> Option<ImageMediaType> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "image/jpeg" | "image/jpg" => Some(ImageMediaType::JPEG),
        "image/png" => Some(ImageMediaType::PNG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        "image/heic" => Some(ImageMediaType::HEIC),
        "image/heif" => Some(ImageMediaType::HEIF),
        "image/svg+xml" => Some(ImageMediaType::SVG),
        _ => None,
    }
}

fn parse_audio_media_type(mime: &str) -> Option<AudioMediaType> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "audio/wav" | "audio/x-wav" => Some(AudioMediaType::WAV),
        "audio/mpeg" => Some(AudioMediaType::MP3),
        "audio/aac" => Some(AudioMediaType::AAC),
        "audio/ogg" | "audio/opus" => Some(AudioMediaType::OGG),
        "audio/flac" => Some(AudioMediaType::FLAC),
        "audio/mp4" | "audio/m4a" => Some(AudioMediaType::M4A),
        _ => None,
    }
}

fn parse_document_media_type(mime: &str) -> Option<DocumentMediaType> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "application/pdf" => Some(DocumentMediaType::PDF),
        "text/plain" => Some(DocumentMediaType::TXT),
        "text/html" => Some(DocumentMediaType::HTML),
        "text/css" => Some(DocumentMediaType::CSS),
        "text/markdown" => Some(DocumentMediaType::MARKDOWN),
        "text/csv" => Some(DocumentMediaType::CSV),
        "application/xml" | "text/xml" => Some(DocumentMediaType::XML),
        "application/javascript" | "text/javascript" => Some(DocumentMediaType::Javascript),
        "text/x-python" | "application/x-python" => Some(DocumentMediaType::Python),
        _ => None,
    }
}

fn b64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

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
///
/// All rates are micro-USD per **1M tokens** (so $3.00 / MTok →
/// `MicroUsd::from_micros(3_000_000)`). Use [`MicroUsd::cost_for_tokens`]
/// to apply these to a token count — that path keeps everything in
/// integer math, no float drift.
///
/// `cached_input_per_1m_tokens` and `cache_write_per_1m_tokens` are
/// `None` when the provider doesn't bill prompt-cache traffic
/// separately (or the snapshot row pre-dates the field). When set,
/// `compute_cost_usd` charges the cached portion of `input_tokens`
/// at the cached rate instead of the full input rate, and bills
/// `cache_creation_input_tokens` at the write rate. Typical multipliers
/// vs. the input rate: 0.1× cached read, 1.25× cache write.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_1m_tokens: aura_model::MicroUsd,
    pub output_per_1m_tokens: aura_model::MicroUsd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_1m_tokens: Option<aura_model::MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_1m_tokens: Option<aura_model::MicroUsd>,
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
///
/// `cached_input_tokens` and `cache_creation_input_tokens` carry
/// Anthropic-style prompt-cache accounting: how many input tokens were
/// served from the provider's prompt cache vs. written into it. Both
/// are 0 when the provider doesn't report cache usage (OpenAI
/// subscription path, providers without prompt caching, etc.).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
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

    /// Wrap an arbitrary event stream into an `LlmStream`. The billing
    /// layer uses this to interpose a recording wrapper over a provider
    /// stream without the consumer seeing a different type.
    pub(crate) fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = crate::Result<StreamEvent>> + Send + 'static,
    {
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
                Err(e) => Some(Err(rig_completion_to_error(e))),
                Ok(event) => convert_stream_event(event),
            })
        });
        Self {
            inner: Box::pin(mapped),
        }
    }

    /// Anthropic-specific variant of [`Self::from_rig_stream`].
    /// Anthropic's API reports `input_tokens / cache_read /
    /// cache_creation` as disjoint buckets summing to the total
    /// prompt; this wrapper folds the cache buckets back into
    /// `input_tokens` so the field uniformly means "total prompt"
    /// across providers (matching OpenAI / Gemini). `cached_input_tokens`
    /// and `cache_creation_input_tokens` stay populated so
    /// `compute_cost_usd` can split them out at billing rates.
    fn from_anthropic_stream(
        rig_stream: streaming::StreamingCompletionResponse<
            anthropic::streaming::StreamingCompletionResponse,
        >,
    ) -> Self {
        let mapped = rig_stream.filter_map(|result| {
            futures::future::ready(match result {
                Err(e) => Some(Err(rig_completion_to_error(e))),
                Ok(event) => match convert_stream_event(event) {
                    Some(Ok(StreamEvent::Usage(mut usage))) => {
                        fold_token_usage_cache_into_total(&mut usage);
                        Some(Ok(StreamEvent::Usage(usage)))
                    }
                    other => other,
                },
            })
        });
        Self {
            inner: Box::pin(mapped),
        }
    }

    /// Gemini-specific variant of [`Self::from_rig_stream`]. rig 0.34's
    /// `GetTokenUsage` impl for Gemini's streaming response leaves
    /// `cached_input_tokens` at 0; this wrapper reads
    /// `usage_metadata.cached_content_token_count` straight off the raw
    /// `Final` payload so prompt-cache hits land in cost_records. Drop
    /// once rig upstream fixes the impl.
    fn from_gemini_stream(
        rig_stream: streaming::StreamingCompletionResponse<
            gemini::streaming::StreamingCompletionResponse,
        >,
    ) -> Self {
        let mapped = rig_stream.filter_map(|result| {
            futures::future::ready(match result {
                Err(e) => Some(Err(rig_completion_to_error(e))),
                Ok(StreamedAssistantContent::Final(r)) => {
                    let mut usage = r.token_usage().unwrap_or_default();
                    usage.cached_input_tokens =
                        r.usage_metadata.cached_content_token_count.unwrap_or(0) as u64;
                    Some(Ok(StreamEvent::Usage(TokenUsage {
                        input_tokens: usage.input_tokens as usize,
                        output_tokens: usage.output_tokens as usize,
                        cached_input_tokens: usage.cached_input_tokens as usize,
                        cache_creation_input_tokens: usage.cache_creation_input_tokens as usize,
                    })))
                }
                Ok(event) => convert_stream_event(event),
            })
        });
        Self {
            inner: Box::pin(mapped),
        }
    }
}

/// Anthropic reports `input_tokens / cache_read / cache_creation` as
/// disjoint buckets summing to the total prompt; OpenAI and Gemini
/// report `input_tokens = total prompt` with cached as a subset.
/// Folds the cache buckets back into `input_tokens` so the field
/// uniformly means "total prompt" downstream — `compute_cost_usd`
/// can run one billing formula across providers.
fn fold_anthropic_cache_into_total(usage: &mut completion::Usage) {
    usage.input_tokens = usage
        .input_tokens
        .saturating_add(usage.cached_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
}

fn fold_token_usage_cache_into_total(usage: &mut TokenUsage) {
    usage.input_tokens = usage
        .input_tokens
        .saturating_add(usage.cached_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
}

fn convert_stream_event<R: GetTokenUsage>(
    event: StreamedAssistantContent<R>,
) -> Option<crate::Result<StreamEvent>> {
    match event {
        StreamedAssistantContent::Text(t) => Some(Ok(StreamEvent::Text(t.text))),
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            Some(Ok(StreamEvent::ToolCall(ToolCallInfo {
                id: tool_call.id,
                name: tool_name::unsanitize_tool_name(&tool_call.function.name),
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
                cached_input_tokens: usage.cached_input_tokens as usize,
                cache_creation_input_tokens: usage.cache_creation_input_tokens as usize,
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
    /// DeepSeek's dedicated provider — round-trips `reasoning_content`
    /// on assistant tool-call turns, which thinking mode requires.
    DeepSeek(deepseek::CompletionModel),
    /// Additional rig-backed providers (see `providers::rig_providers`).
    /// All speak through rig's uniform completion model, so they share the
    /// generic `from_rig_stream` path.
    Xai(xai::completion::CompletionModel),
    Mistral(mistral::completion::CompletionModel),
    Cohere(cohere::CompletionModel),
    Perplexity(perplexity::CompletionModel),
    Moonshot(moonshot::CompletionModel),
    Zai(openai::completion::GenericCompletionModel<zai::ZAiExt>),
    XiaomiMimo(openai::completion::GenericCompletionModel<xiaomimimo::XiaomiMimoExt>),
    /// MiniMax via rig's dedicated provider on its Anthropic-compatible
    /// surface — shares Anthropic's cache-bucket folding and stream path.
    Minimax(anthropic::completion::GenericCompletionModel<minimax::MiniMaxAnthropicExt>),
    /// Inference hosts (see `providers::rig_providers`). Also rig-backed,
    /// so they share the generic `from_rig_stream` path.
    Groq(groq::CompletionModel),
    Together(together::completion::CompletionModel),
    Ollama(ollama::CompletionModel),
    Llamafile(llamafile::CompletionModel),
    Hyperbolic(hyperbolic::CompletionModel),
    HuggingFace(huggingface::completion::CompletionModel),
    /// ChatGPT/Codex OAuth subscription path. Doesn't go through rig — the
    /// Codex Responses API uses a different request shape and needs custom
    /// auth + 401-refresh handling. See `providers::openai_subscription`.
    OpenAiSubscription(crate::providers::openai_subscription::OpenAiSubscriptionCompletionModel),
}

/// Erase a provider `CompletionResponse<R>`'s raw-response payload for the
/// agent loop. For OpenAI-compatible rig providers that need no per-field
/// usage massaging (unlike Anthropic's cache folding / Gemini's cache
/// re-derivation).
fn repack_completion<R>(
    resp: completion::CompletionResponse<R>,
) -> completion::CompletionResponse<()> {
    completion::CompletionResponse {
        choice: resp.choice,
        usage: resp.usage,
        raw_response: (),
        message_id: resp.message_id,
    }
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
                let mut usage = resp.usage;
                fold_anthropic_cache_into_total(&mut usage);
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::Gemini(m) => {
                let resp = m.completion(request).await?;
                // rig 0.34's Gemini path leaves `cached_input_tokens` at 0
                // even when the response carries `cachedContentTokenCount`.
                // Re-derive it from the raw `usage_metadata` so prompt-cache
                // hits land in cost_records correctly. Drop this once rig
                // upstream populates the field itself.
                let mut usage = resp.usage;
                if let Some(meta) = resp.raw_response.usage_metadata.as_ref() {
                    usage.cached_input_tokens = meta.cached_content_token_count.unwrap_or(0) as u64;
                }
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::DeepSeek(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Xai(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Mistral(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Cohere(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Perplexity(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Moonshot(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Zai(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::XiaomiMimo(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Minimax(m) => {
                let resp = m.completion(request).await?;
                let mut usage = resp.usage;
                fold_anthropic_cache_into_total(&mut usage);
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::Groq(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Together(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Ollama(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Llamafile(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Hyperbolic(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::HuggingFace(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::OpenAiSubscription(m) => m.completion(request).await,
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
                Ok(LlmStream::from_anthropic_stream(stream))
            }
            Self::Gemini(m) => {
                let stream = m.stream(request).await?;
                Ok(LlmStream::from_gemini_stream(stream))
            }
            Self::DeepSeek(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Xai(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Mistral(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Cohere(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Perplexity(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Moonshot(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Zai(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::XiaomiMimo(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Minimax(m) => Ok(LlmStream::from_anthropic_stream(m.stream(request).await?)),
            Self::Groq(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Together(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Ollama(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Llamafile(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Hyperbolic(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::HuggingFace(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::OpenAiSubscription(m) => m.stream(request).await,
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

/// Capability the LLM client uses to materialise blob bytes for
/// multimodal user content. Decoupled from `aura-storage::BlobStore`
/// so the LLM crate stays storage-agnostic — the agent runtime wraps
/// its `BlobStore` in an adapter that implements this trait.
#[async_trait::async_trait]
pub trait BlobFetcher: Send + Sync {
    async fn fetch(&self, blob_id: &str) -> Result<Vec<u8>>;
}

/// The main LLM client type wrapping a rig completion model.
pub struct LlmClient {
    pub(crate) model_info: ModelInfo,
    model: AnyCompletionModel,
    /// Optional blob fetcher. When set AND `model_info.supports_vision`
    /// is true, `ContentBlock::Image` / `Audio` / `File` user-content
    /// blocks are materialised into proper rig `Image` / `Audio` /
    /// `Document` content (base64-encoded bytes). Without it, every
    /// non-text block degrades to a `[image: …]`-style text stub —
    /// the model never sees the actual payload.
    blob_fetcher: Option<std::sync::Arc<dyn BlobFetcher>>,
}

impl LlmClient {
    /// Creates a new `LlmClient` from a provider-specific completion model.
    pub(crate) fn new(model_info: ModelInfo, model: AnyCompletionModel) -> Self {
        Self {
            model_info,
            model,
            blob_fetcher: None,
        }
    }

    /// Attach a blob fetcher so the client can materialise multimodal
    /// content. Required for `supports_vision: true` to actually mean
    /// "image bytes flow through" — without it, the build path falls
    /// back to a text stub even on vision-capable models.
    pub fn with_blob_fetcher(mut self, fetcher: std::sync::Arc<dyn BlobFetcher>) -> Self {
        self.blob_fetcher = Some(fetcher);
        self
    }

    /// Sends a chat request to the provider and returns a unified response.
    pub async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        debug!(
            provider = %self.model_info.provider,
            model = %self.model_info.id,
            "sending chat request"
        );

        let rig_request = self.build_completion_request(request).await;

        let response = self
            .model
            .completion(rig_request)
            .await
            .map_err(rig_completion_to_error)?;

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

        let rig_request = self.build_completion_request(request).await;

        let stream = self
            .model
            .stream(rig_request)
            .await
            .map_err(rig_completion_to_error)?;

        Ok(stream)
    }

    /// Build a rig `CompletionRequest` from our `ChatRequest`.
    ///
    /// Async because vision-capable models need their `Image` /
    /// `Audio` / `File` content blocks materialised from the blob
    /// store — the LLM call is the async boundary closest to the
    /// API hop, and base64-encoding a 100 MiB blob inline would
    /// stall the runtime if we did it synchronously.
    async fn build_completion_request(&self, request: &ChatRequest) -> CompletionRequest {
        let mut system_parts = Vec::new();
        let mut chat_messages: Vec<Message> = Vec::new();

        for msg in &request.messages {
            match msg.role {
                aura_model::Role::System => {
                    system_parts.push(multimodal::extract_text(&msg.content));
                }
                aura_model::Role::User => {
                    let mut parts: Vec<UserContent> = Vec::with_capacity(msg.content.len());
                    for block in &msg.content {
                        parts.push(self.user_content_for_block(block).await);
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
                                            name: tool_name::sanitize_tool_name(name),
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
                name: tool_name::sanitize_tool_name(&t.name),
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

    /// Convert one user-side `ContentBlock` into a rig `UserContent`.
    /// Multimodal blocks become real `Image` / `Audio` / `Document`
    /// content when (1) the model claims vision support and (2) a
    /// blob fetcher is wired in. Otherwise — including when blob
    /// fetch fails — the block degrades to a `[image: …]`-style text
    /// stub so the conversation can keep going even if the bytes
    /// aren't deliverable.
    async fn user_content_for_block(&self, block: &aura_model::ContentBlock) -> UserContent {
        match block {
            aura_model::ContentBlock::Text(t) => UserContent::Text(Text { text: t.clone() }),
            aura_model::ContentBlock::Image { blob, mime_type }
                if self.model_info.supports_vision =>
            {
                if let (Some(fetcher), Some(media_type)) = (
                    self.blob_fetcher.as_ref(),
                    parse_image_media_type(mime_type),
                ) {
                    match fetcher.fetch(&blob.blob_id).await {
                        Ok(bytes) => UserContent::Image(Image {
                            data: DocumentSourceKind::Base64(b64_encode(&bytes)),
                            media_type: Some(media_type),
                            // Required by the OpenAI-compat converter when
                            // the source is a Base64 data URL — without
                            // it rig errors with "image URI must have
                            // image detail" before the request leaves
                            // the client. `Auto` is OpenAI's default
                            // when the field is absent on text-only id
                            // URLs anyway, so it's the lossless choice.
                            detail: Some(ImageDetail::Auto),
                            additional_params: None,
                        }),
                        Err(e) => {
                            tracing::warn!(
                                blob_id = %blob.blob_id,
                                error = %e,
                                "blob fetch failed; falling back to text stub for image",
                            );
                            UserContent::Text(Text {
                                text: multimodal::content_block_to_text(block),
                            })
                        }
                    }
                } else {
                    UserContent::Text(Text {
                        text: multimodal::content_block_to_text(block),
                    })
                }
            }
            aura_model::ContentBlock::Audio { blob, mime_type }
                if self.model_info.supports_vision =>
            {
                if let (Some(fetcher), Some(media_type)) = (
                    self.blob_fetcher.as_ref(),
                    parse_audio_media_type(mime_type),
                ) {
                    match fetcher.fetch(&blob.blob_id).await {
                        Ok(bytes) => UserContent::Audio(Audio {
                            data: DocumentSourceKind::Base64(b64_encode(&bytes)),
                            media_type: Some(media_type),
                            additional_params: None,
                        }),
                        Err(e) => {
                            tracing::warn!(
                                blob_id = %blob.blob_id,
                                error = %e,
                                "blob fetch failed; falling back to text stub for audio",
                            );
                            UserContent::Text(Text {
                                text: multimodal::content_block_to_text(block),
                            })
                        }
                    }
                } else {
                    UserContent::Text(Text {
                        text: multimodal::content_block_to_text(block),
                    })
                }
            }
            aura_model::ContentBlock::File {
                blob, mime_type, ..
            } if self.model_info.supports_vision => {
                if let (Some(fetcher), Some(media_type)) = (
                    self.blob_fetcher.as_ref(),
                    parse_document_media_type(mime_type),
                ) {
                    match fetcher.fetch(&blob.blob_id).await {
                        Ok(bytes) => UserContent::Document(Document {
                            data: DocumentSourceKind::Base64(b64_encode(&bytes)),
                            media_type: Some(media_type),
                            additional_params: None,
                        }),
                        Err(e) => {
                            tracing::warn!(
                                blob_id = %blob.blob_id,
                                error = %e,
                                "blob fetch failed; falling back to text stub for document",
                            );
                            UserContent::Text(Text {
                                text: multimodal::content_block_to_text(block),
                            })
                        }
                    }
                } else {
                    UserContent::Text(Text {
                        text: multimodal::content_block_to_text(block),
                    })
                }
            }
            other => UserContent::Text(Text {
                text: multimodal::content_block_to_text(other),
            }),
        }
    }

    /// Convert a rig `CompletionResponse` into our `LlmResponse`.
    fn convert_response(&self, response: completion::CompletionResponse<()>) -> LlmResponse {
        let mut content = String::new();
        let mut content_blocks = Vec::new();
        let mut tool_calls = Vec::new();
        let mut thinking: Option<String> = None;

        for item in response.choice.into_iter() {
            match item {
                AssistantContent::Text(text) => {
                    if !text.text.is_empty() {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&text.text);
                        content_blocks.push(aura_model::ContentBlock::Text(text.text));
                    }
                }
                AssistantContent::ToolCall(tc) => {
                    tool_calls.push(ToolCallInfo {
                        id: tc.id,
                        name: tool_name::unsanitize_tool_name(&tc.function.name),
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
                    content_blocks.push(convert_reasoning_to_block(r));
                }
                AssistantContent::Image(_) => {}
            }
        }

        let usage = TokenUsage {
            input_tokens: response.usage.input_tokens as usize,
            output_tokens: response.usage.output_tokens as usize,
            cached_input_tokens: response.usage.cached_input_tokens as usize,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens as usize,
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
            messages: vec![aura_model::ChatMessage::agent_context(vec![
                aura_model::ContentBlock::Text("ping".to_string()),
            ])],
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
            thinking_chars: response.thinking.as_ref().map(|s| s.chars().count()),
            thinking_preview: response
                .thinking
                .as_ref()
                .map(|s| s.lines().next().unwrap_or("").chars().take(120).collect()),
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
    /// Number of UTF-8 characters of reasoning summary the provider
    /// returned. `None` when the model didn't emit any reasoning —
    /// either because reasoning is disabled or because the provider
    /// doesn't support it. Useful for `aura llm probe` to confirm
    /// reasoning is actually flowing end-to-end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_chars: Option<usize>,
    /// First line of the reasoning summary, truncated to 120 chars,
    /// for human-readable verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_preview: Option<String>,
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

#[cfg(test)]
mod multimodal_dispatch_tests {
    //! Coverage for `user_content_for_block` — the place where
    //! `ContentBlock::Image` either becomes a real `UserContent::Image`
    //! (bytes the model can actually see) or degrades to a text stub.
    //! The image-delivery bug we're fixing here was exactly the second
    //! branch firing on a vision-capable model because no `BlobFetcher`
    //! was wired in.

    use std::sync::Arc;

    use aura_model::{BlobRef, ContentBlock};
    use rig::completion::message::{DocumentSourceKind, ImageMediaType};
    use rig::message::UserContent;

    use super::*;
    use crate::registry::LlmProviderRegistry;

    struct StaticFetcher(Vec<u8>);

    #[async_trait::async_trait]
    impl BlobFetcher for StaticFetcher {
        async fn fetch(&self, _blob_id: &str) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    struct FailingFetcher;

    #[async_trait::async_trait]
    impl BlobFetcher for FailingFetcher {
        async fn fetch(&self, blob_id: &str) -> Result<Vec<u8>> {
            Err(LlmError::Transient(format!("nope: {blob_id}")))
        }
    }

    fn vision_client() -> LlmClient {
        // Factory route: any provider produces an `LlmClient` shape we
        // can mutate. MiniMax's factory default is `supports_vision:
        // false` (the M2 family is text-first), so we use the config
        // override path to force vision on for the test fixture —
        // exactly the same way an operator on MiniMax-VL-01 would.
        let registry = LlmProviderRegistry::with_default_providers();
        registry
            .build_client(&LlmProviderConfig {
                provider: "minimax".into(),
                api_key: Some("test".into()),
                base_url: None,
                model: "MiniMax-VL-01".into(),
                supports_vision: Some(true),
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
                proxy: None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn image_block_with_fetcher_emits_real_image_content() {
        let bytes = b"\xFF\xD8\xFFhello-jpeg-bytes".to_vec();
        let client = vision_client().with_blob_fetcher(Arc::new(StaticFetcher(bytes.clone())));
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:deadbeef.tok".into(),
            },
            mime_type: "image/jpeg".into(),
        };
        let out = client.user_content_for_block(&block).await;
        match out {
            UserContent::Image(img) => {
                assert_eq!(img.media_type, Some(ImageMediaType::JPEG));
                // Required by rig's OpenAI converter — Base64 source
                // without an image detail errors at message-build time.
                assert_eq!(
                    img.detail,
                    Some(rig::completion::message::ImageDetail::Auto)
                );
                let DocumentSourceKind::Base64(b64) = img.data else {
                    panic!("expected base64 data");
                };
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .unwrap();
                assert_eq!(decoded, bytes);
            }
            other => panic!("expected Image variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_block_without_fetcher_falls_back_to_text_stub() {
        // Same vision-capable model, but no blob fetcher attached →
        // the model would have to invent the bytes, which is exactly
        // the bug; assert we degrade explicitly to a text stub.
        let client = vision_client();
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:abc.tok".into(),
            },
            mime_type: "image/jpeg".into(),
        };
        let out = client.user_content_for_block(&block).await;
        match out {
            UserContent::Text(t) => {
                assert!(t.text.contains("[image:"), "stub form, got {}", t.text);
                assert!(t.text.contains("sha256:abc.tok"));
            }
            other => panic!("expected Text fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetcher_error_falls_back_to_text_stub_not_panic() {
        // A blob the gateway can't read (missing, deleted, etc.)
        // must not blow up the LLM call — drop to the same text stub
        // an unconfigured fetcher would emit, with a tracing warn so
        // operators can find it.
        let client = vision_client().with_blob_fetcher(Arc::new(FailingFetcher));
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:gone.tok".into(),
            },
            mime_type: "image/png".into(),
        };
        let out = client.user_content_for_block(&block).await;
        assert!(matches!(out, UserContent::Text(_)));
    }

    #[tokio::test]
    async fn image_block_on_text_only_model_skips_fetcher_and_stubs() {
        // Even with a fetcher attached, a model that doesn't claim
        // vision must NOT pull bytes — otherwise we'd waste 100 MiB
        // of base64 work just to send something the API will reject.
        let bytes = b"unused".to_vec();
        let registry = LlmProviderRegistry::with_default_providers();
        let mut client = registry
            .build_client(&LlmProviderConfig {
                provider: "openai".into(),
                api_key: Some("test".into()),
                base_url: None,
                model: "gpt-3.5-turbo".into(),
                supports_vision: None,
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
                proxy: None,
            })
            .unwrap();
        // OpenAI factory may set vision=true on some models; force
        // false here so the test pins the dispatch rule, not the
        // model_info defaults.
        client.model_info.supports_vision = false;
        let client = client.with_blob_fetcher(Arc::new(StaticFetcher(bytes)));
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:never-fetched.tok".into(),
            },
            mime_type: "image/jpeg".into(),
        };
        let out = client.user_content_for_block(&block).await;
        assert!(matches!(out, UserContent::Text(_)));
    }

    #[test]
    fn parse_image_media_type_recognises_common_mimes() {
        assert_eq!(
            parse_image_media_type("image/jpeg"),
            Some(ImageMediaType::JPEG)
        );
        assert_eq!(
            parse_image_media_type("IMAGE/PNG"),
            Some(ImageMediaType::PNG)
        );
        assert_eq!(
            parse_image_media_type("image/jpeg; charset=binary"),
            Some(ImageMediaType::JPEG),
        );
        assert_eq!(parse_image_media_type("image/x-fancy"), None);
    }
}

#[cfg(test)]
mod cost_normalization_tests {
    //! Pin: the Anthropic adapter folds disjoint cache buckets back
    //! into `input_tokens` so downstream `compute_cost_usd` can use
    //! one billing formula across providers. If this regresses,
    //! Anthropic-cache workflows under-bill the cached/cache-write
    //! portions.

    use super::*;

    #[test]
    fn fold_token_usage_adds_cache_buckets_into_total() {
        let mut u = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 50,
            cache_creation_input_tokens: 30,
        };
        fold_token_usage_cache_into_total(&mut u);
        assert_eq!(u.input_tokens, 180);
        assert_eq!(u.cached_input_tokens, 50);
        assert_eq!(u.cache_creation_input_tokens, 30);
        assert_eq!(u.output_tokens, 20);
    }

    #[test]
    fn fold_token_usage_is_idempotent_on_zero_cache_buckets() {
        let mut u = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        fold_token_usage_cache_into_total(&mut u);
        assert_eq!(u.input_tokens, 100);
    }
}
