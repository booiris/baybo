mod error;
pub mod multimodal;
mod providers;
pub mod registry;
pub mod rig_adapter;
pub mod tool_call_extractor;

use serde::{Deserialize, Serialize};
use tracing::debug;

pub use crate::error::LlmError;
pub use crate::registry::{LlmProviderConfig, LlmProviderRegistry};
pub use crate::tool_call_extractor::JsonExtractor;

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
    http_client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
}

impl LlmClient {
    /// Creates a new `LlmClient` with the given model info, parse mode, and connection details.
    pub fn new(
        model_info: ModelInfo,
        parse_mode: ResponseParseMode,
        api_key: Option<String>,
        base_url: String,
    ) -> Self {
        Self {
            model_info,
            parse_mode,
            http_client: reqwest::Client::new(),
            api_key,
            base_url,
        }
    }

    /// Sends a chat request to the provider and returns a unified response.
    pub async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        let body = self.build_provider_request_body(request);
        let provider = self.model_info.provider.as_str();

        debug!(provider, model = %self.model_info.id, "sending chat request");

        let response_json = self.send_request(provider, &body).await?;

        let response = match provider {
            "anthropic" => self.parse_anthropic_response(&response_json, request),
            "ollama" => self.parse_ollama_response(&response_json, request),
            _ => self.parse_openai_response(&response_json, request),
        }?;

        debug!(
            content_len = response.content.len(),
            tool_calls = response.tool_calls.len(),
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "received LLM response"
        );

        Ok(response)
    }

    /// Send the HTTP request to the appropriate provider endpoint.
    async fn send_request(
        &self,
        provider: &str,
        body: &serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        let (url, mut req_builder) = match provider {
            "anthropic" => {
                let url = format!("{}/v1/messages", self.base_url);
                let builder = self
                    .http_client
                    .post(&url)
                    .header("x-api-key", self.api_key.as_deref().unwrap_or_default())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                (url, builder)
            }
            "ollama" => {
                let url = format!("{}/api/chat", self.base_url);
                let builder = self
                    .http_client
                    .post(&url)
                    .header("content-type", "application/json");
                (url, builder)
            }
            _ => {
                // OpenAI and OpenAI-compatible
                let url = format!("{}/chat/completions", self.base_url);
                let builder = self
                    .http_client
                    .post(&url)
                    .header("content-type", "application/json")
                    .bearer_auth(self.api_key.as_deref().unwrap_or_default());
                (url, builder)
            }
        };

        req_builder = req_builder.json(body);

        let resp = req_builder
            .send()
            .await
            .map_err(|e| LlmError::Provider(format!("HTTP request to {url} failed: {e}")))?;

        let status = resp.status();
        let resp_body = resp
            .text()
            .await
            .map_err(|e| LlmError::Provider(format!("failed to read response body: {e}")))?;

        if !status.is_success() {
            return Err(LlmError::Provider(format!(
                "LLM API returned {status}: {resp_body}"
            )));
        }

        serde_json::from_str(&resp_body)
            .map_err(|e| LlmError::ParseError(format!("failed to parse response JSON: {e}")))
    }

    /// Parse an OpenAI-format response.
    fn parse_openai_response(
        &self,
        json: &serde_json::Value,
        _request: &ChatRequest,
    ) -> crate::Result<LlmResponse> {
        let choice = json
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"));

        let content = choice
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        let content_blocks = if content.is_empty() {
            vec![]
        } else {
            vec![aura_model::ContentBlock::Text(content.clone())]
        };

        // Extract tool calls
        let mut tool_calls = Vec::new();
        if let Some(calls) = choice
            .and_then(|m| m.get("tool_calls"))
            .and_then(|t| t.as_array())
        {
            for call in calls {
                let id = call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let arguments = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                tool_calls.push(ToolCallInfo {
                    id,
                    name,
                    arguments,
                });
            }
        }

        // If no native tool calls found, try prompt-guided extraction
        if tool_calls.is_empty()
            && matches!(&self.parse_mode, ResponseParseMode::PromptGuided { .. })
        {
            let extractor = tool_call_extractor::JsonExtractor::new();
            tool_calls = extractor.extract(&content);
        }

        let usage = Self::extract_usage(json);

        Ok(LlmResponse {
            content,
            content_blocks,
            tool_calls,
            usage,
            thinking: None,
        })
    }

    /// Parse an Anthropic-format response.
    fn parse_anthropic_response(
        &self,
        json: &serde_json::Value,
        _request: &ChatRequest,
    ) -> crate::Result<LlmResponse> {
        let mut content = String::new();
        let mut content_blocks = Vec::new();
        let mut tool_calls = Vec::new();
        let mut thinking = None;

        if let Some(blocks) = json.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        let text = block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default();
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(text);
                        content_blocks.push(aura_model::ContentBlock::Text(text.to_string()));
                    }
                    "tool_use" => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let arguments = block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        tool_calls.push(ToolCallInfo {
                            id,
                            name,
                            arguments,
                        });
                    }
                    "thinking" => {
                        if let Some(text) = block.get("thinking").and_then(|t| t.as_str()) {
                            thinking = Some(text.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }

        let usage = Self::extract_usage(json);

        Ok(LlmResponse {
            content,
            content_blocks,
            tool_calls,
            usage,
            thinking,
        })
    }

    /// Parse an Ollama-format response.
    fn parse_ollama_response(
        &self,
        json: &serde_json::Value,
        _request: &ChatRequest,
    ) -> crate::Result<LlmResponse> {
        let content = json
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        let content_blocks = if content.is_empty() {
            vec![]
        } else {
            vec![aura_model::ContentBlock::Text(content.clone())]
        };

        // Ollama doesn't natively support tool calls; use prompt-guided extraction
        let tool_calls = if let ResponseParseMode::PromptGuided { .. } = &self.parse_mode {
            let extractor = tool_call_extractor::JsonExtractor::new();
            extractor.extract(&content)
        } else {
            Vec::new()
        };

        // Ollama provides eval_count / prompt_eval_count
        let input_tokens = json
            .get("prompt_eval_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let output_tokens = json.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        Ok(LlmResponse {
            content,
            content_blocks,
            tool_calls,
            usage: TokenUsage {
                input_tokens,
                output_tokens,
            },
            thinking: None,
        })
    }

    /// Extract token usage from a response JSON (OpenAI / Anthropic format).
    fn extract_usage(json: &serde_json::Value) -> TokenUsage {
        let usage = json.get("usage");
        let input_tokens = usage
            .and_then(|u| u.get("input_tokens").or_else(|| u.get("prompt_tokens")))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let output_tokens = usage
            .and_then(|u| {
                u.get("output_tokens")
                    .or_else(|| u.get("completion_tokens"))
            })
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        TokenUsage {
            input_tokens,
            output_tokens,
        }
    }

    /// Build a provider-specific request body from a generic `ChatRequest`.
    ///
    /// This converts tool definitions and message content blocks into the JSON
    /// format expected by the target provider (OpenAI, Anthropic, etc.).
    pub fn build_provider_request_body(&self, request: &ChatRequest) -> serde_json::Value {
        let provider = &self.model_info.provider;

        match provider.as_str() {
            "anthropic" => self.build_anthropic_request_body(request),
            "ollama" => self.build_ollama_request_body(request),
            _ => self.build_openai_request_body(request),
        }
    }

    fn build_openai_request_body(&self, request: &ChatRequest) -> serde_json::Value {
        let tools_json =
            serde_json::to_value(rig_adapter::to_openai_tools(&request.tools)).unwrap_or_default();

        let messages_json: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|msg| {
                let role_str = match msg.role {
                    aura_model::Role::System => "system",
                    aura_model::Role::User => "user",
                    aura_model::Role::Assistant => "assistant",
                    aura_model::Role::Tool => "tool",
                };
                let content = serde_json::to_value(multimodal::to_openai_content(&msg.content))
                    .unwrap_or_default();
                serde_json::json!({ "role": role_str, "content": content })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model_info.id,
            "messages": messages_json,
        });
        if !request.tools.is_empty() {
            body["tools"] = tools_json;
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        body
    }

    fn build_anthropic_request_body(&self, request: &ChatRequest) -> serde_json::Value {
        let tools_json = serde_json::to_value(rig_adapter::to_anthropic_tools(&request.tools))
            .unwrap_or_default();

        // Anthropic requires system prompt separate from messages
        let mut system_prompt = String::new();
        let messages_json: Vec<serde_json::Value> = request
            .messages
            .iter()
            .filter_map(|msg| {
                if msg.role == aura_model::Role::System {
                    let text = multimodal::extract_text(&msg.content);
                    if !system_prompt.is_empty() {
                        system_prompt.push('\n');
                    }
                    system_prompt.push_str(&text);
                    return None;
                }
                let role_str = match msg.role {
                    aura_model::Role::User => "user",
                    aura_model::Role::Assistant => "assistant",
                    aura_model::Role::Tool => "user", // Anthropic maps tool results as user
                    aura_model::Role::System => unreachable!(),
                };
                let content = serde_json::to_value(multimodal::to_anthropic_content(&msg.content))
                    .unwrap_or_default();
                Some(serde_json::json!({ "role": role_str, "content": content }))
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model_info.id,
            "messages": messages_json,
            "max_tokens": 4096,
        });
        if !system_prompt.is_empty() {
            body["system"] = serde_json::json!(system_prompt);
        }
        if !request.tools.is_empty() {
            body["tools"] = tools_json;
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        body
    }

    fn build_ollama_request_body(&self, request: &ChatRequest) -> serde_json::Value {
        // For prompt-guided mode, inject tool schema into system prompt
        let tool_schema_prompt = if let ResponseParseMode::PromptGuided {
            tool_schema_prompt, ..
        } = &self.parse_mode
        {
            if !request.tools.is_empty() {
                // Rebuild with actual tools
                rig_adapter::build_tool_schema_prompt(&request.tools)
            } else {
                tool_schema_prompt.clone()
            }
        } else {
            String::new()
        };

        let messages_json: Vec<serde_json::Value> = request
            .messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                let role_str = match msg.role {
                    aura_model::Role::System => "system",
                    aura_model::Role::User => "user",
                    aura_model::Role::Assistant => "assistant",
                    aura_model::Role::Tool => "user",
                };
                let mut text = multimodal::extract_text(&msg.content);
                // Append tool schema to first system message
                if i == 0 && msg.role == aura_model::Role::System && !tool_schema_prompt.is_empty()
                {
                    text.push_str("\n\n");
                    text.push_str(&tool_schema_prompt);
                }
                serde_json::json!({ "role": role_str, "content": text })
            })
            .collect();

        serde_json::json!({
            "model": self.model_info.id,
            "messages": messages_json,
            "stream": false,
        })
    }

    /// Returns the model identifier (e.g. `"claude-sonnet-4-6"`).
    pub fn model_id(&self) -> &str {
        &self.model_info.id
    }

    /// Returns the full model metadata.
    pub fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }
}
