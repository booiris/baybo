pub mod multimodal;
pub mod prompt_guided;
pub mod providers;
pub mod registry;
pub mod rig_adapter;

use serde::{Deserialize, Serialize};

pub use crate::prompt_guided::JsonExtractor;
pub use crate::providers::anthropic::AnthropicProviderFactory;
pub use crate::providers::ollama::OllamaProviderFactory;
pub use crate::providers::openai::OpenAIProviderFactory;
pub use crate::registry::{LlmProviderConfig, LlmProviderFactory, LlmProviderRegistry};

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
    pub content_blocks: Vec<aura_core::ContentBlock>,
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
    pub messages: Vec<aura_core::ChatMessage>,
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

/// Controls how tool calls are extracted from LLM responses.
#[derive(Debug, Clone)]
pub enum ResponseParseMode {
    /// The model natively supports function calling.
    NativeFunctionCalling,
    /// The model requires prompt-guided JSON extraction for tool calls.
    PromptGuided { tool_schema_prompt: String },
}

/// The main LLM client type wrapping a model instance and its metadata.
pub struct LlmClient {
    model_info: ModelInfo,
    parse_mode: ResponseParseMode,
}

impl LlmClient {
    /// Creates a new `LlmClient` with the given model info and parse mode.
    pub fn new(model_info: ModelInfo, parse_mode: ResponseParseMode) -> Self {
        Self {
            model_info,
            parse_mode,
        }
    }

    /// Sends a chat request and returns a unified response.
    ///
    /// This is currently a stub. Provider HTTP backends will be wired in a
    /// future phase; for now the client is useful for metadata queries and
    /// registry testing.
    pub async fn chat(&self, _request: &ChatRequest) -> aura_core::Result<LlmResponse> {
        Err(aura_core::AuraError::Internal(anyhow::anyhow!(
            "LlmClient::chat() is not yet implemented — \
             provider HTTP backends will be added in a future phase"
        )))
    }

    /// Returns the model identifier (e.g. `"claude-sonnet-4-6"`).
    pub fn model_id(&self) -> &str {
        &self.model_info.id
    }

    /// Returns the full model metadata.
    pub fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }

    /// Returns the current response parse mode.
    pub fn parse_mode(&self) -> &ResponseParseMode {
        &self.parse_mode
    }
}
