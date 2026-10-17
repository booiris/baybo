use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use rig::client::CompletionClient;
use rig::providers::{anthropic, gemini, openai};
use serde::Deserialize;

use crate::effort::EffortWire;
use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, LlmError, ModelInfo};

pub(crate) const ZEN_PROVIDER_NAME: &str = "opencode";
pub(crate) const GO_PROVIDER_NAME: &str = "opencode-go";
const SESSION_HEADER: &str = "x-opencode-session";
const CLIENT_USER_AGENT: &str = concat!("baybo/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpencodeProviderFactory {
    Zen,
    Go,
}

impl OpencodeProviderFactory {
    fn base_url(self) -> &'static str {
        match self {
            Self::Zen => "https://opencode.ai/inference",
            Self::Go => "https://opencode.ai/inference/go",
        }
    }

    fn api_key_env(self) -> &'static str {
        match self {
            Self::Zen => "OPENCODE_API_KEY",
            Self::Go => "OPENCODE_GO_API_KEY",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    ChatCompletions,
    Responses,
    Messages,
    Gemini,
}

impl Protocol {
    fn for_model(plan: OpencodeProviderFactory, model: &str) -> crate::Result<Self> {
        // OpenCode's catalog lists ids without their wire protocol. These
        // routes follow the Console's per-model endpoint tables, including
        // the models whose Zen and Go endpoints differ.
        if model.starts_with("jev-") {
            return Err(LlmError::Config(
                "OpenCode Jev models evaluate structured decisions and do not support chat".into(),
            ));
        }
        if ["gpt-", "grok-", "muse-spark-"]
            .iter()
            .any(|p| model.starts_with(p))
        {
            return Ok(Self::Responses);
        }
        if model.starts_with("gemini-") {
            return Ok(Self::Gemini);
        }
        if model.starts_with("claude-")
            || ["qwen3.5-", "qwen3.6-", "qwen3.7-", "qwen3.8-"]
                .iter()
                .any(|p| model.starts_with(p))
                && (plan == OpencodeProviderFactory::Go || model != "qwen3.8-max")
            || plan == OpencodeProviderFactory::Go && model == "minimax-m3"
        {
            return Ok(Self::Messages);
        }
        Ok(Self::ChatCompletions)
    }

    fn base_path(self) -> &'static str {
        match self {
            Self::ChatCompletions | Self::Responses => "openai/v1",
            // rig appends /v1/messages and /v1beta/models respectively.
            Self::Messages => "anthropic",
            Self::Gemini => "google",
        }
    }

    fn effort_wire(self) -> EffortWire {
        match self {
            Self::ChatCompletions => EffortWire::OpenAiCompatible,
            Self::Responses => EffortWire::OpenAiResponses,
            Self::Messages => EffortWire::Anthropic,
            Self::Gemini => EffortWire::Gemini,
        }
    }

    fn supports_vision(self, model: &str) -> bool {
        match self {
            Self::Responses | Self::Gemini => true,
            Self::Messages => model.starts_with("claude-"),
            Self::ChatCompletions => false,
        }
    }
}

#[async_trait::async_trait]
impl LlmProviderFactory for OpencodeProviderFactory {
    fn provider_name(&self) -> &'static str {
        match self {
            Self::Zen => ZEN_PROVIDER_NAME,
            Self::Go => GO_PROVIDER_NAME,
        }
    }

    fn default_base_url(&self) -> Option<&'static str> {
        Some(self.base_url())
    }

    fn default_api_key_env(&self) -> Option<&'static str> {
        Some(self.api_key_env())
    }

    fn effort_wire(&self, model: &str) -> EffortWire {
        Protocol::for_model(*self, model)
            .map(Protocol::effort_wire)
            .unwrap_or(EffortWire::Unwired)
    }

    fn model_defaults(&self, model: &str) -> crate::FactoryDefaults {
        let mut defaults = crate::providers::factory_defaults_for(self.provider_name());
        defaults.supports_vision =
            Protocol::for_model(*self, model).is_ok_and(|protocol| protocol.supports_vision(model));
        defaults
    }

    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let plan = *self;
        let protocol = Protocol::for_model(plan, &config.model)?;
        let model = OpencodeCompletionModel {
            protocol,
            api_key: api_key(config)?.to_string(),
            base_url: base_url(plan, config),
            model: config.model.clone(),
            http_client: crate::proxied_client(config.proxy.as_ref())?,
            fallback_session: uuid::Uuid::new_v4().to_string(),
        };
        model.for_session(None)?;
        let defaults = self.model_defaults(&config.model);
        let info = ModelInfo {
            id: config.model.clone(),
            provider: plan.provider_name().to_string(),
            context_window: defaults.context_window,
            supports_tools: true,
            supports_vision: defaults.supports_vision,
            pricing: self.pricing_for_model(&config.model),
        };
        Ok(LlmClient::new(
            info,
            AnyCompletionModel::Opencode(model),
            protocol.effort_wire(),
        ))
    }

    async fn live_models(&self, config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
        let plan = *self;
        let response = crate::proxied_client(config.proxy.as_ref())?
            .get(format!("{}/v1/models", base_url(plan, config)))
            .bearer_auth(api_key(config)?)
            .headers(request_headers(&uuid::Uuid::new_v4().to_string())?)
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "OpenCode GET /v1/models"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(crate::status_to_error(
                status.as_u16(),
                format!("OpenCode models returned {status}: {body}"),
            ));
        }
        let payload: ModelList = response
            .json()
            .await
            .map_err(|e| LlmError::Decode(format!("OpenCode models: {e}")))?;
        let mut models: Vec<_> = payload
            .data
            .into_iter()
            .filter_map(|model| {
                Protocol::for_model(plan, &model.id)
                    .ok()
                    .map(|protocol| LiveModelInfo {
                        supports_vision: protocol.supports_vision(&model.id).then_some(true),
                        id: model.id,
                        supports_tools: Some(true),
                        ..Default::default()
                    })
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models.dedup_by(|a, b| a.id == b.id);
        Ok(models)
    }
}

fn api_key(config: &LlmProviderConfig) -> crate::Result<&str> {
    config
        .api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| LlmError::Config(format!("{} requires an API key", config.provider)))
}

fn base_url(plan: OpencodeProviderFactory, config: &LlmProviderConfig) -> String {
    config
        .base_url
        .as_deref()
        .unwrap_or(plan.base_url())
        .trim_end_matches('/')
        .to_string()
}

pub(crate) struct OpencodeCompletionModel {
    protocol: Protocol,
    api_key: String,
    base_url: String,
    model: String,
    http_client: reqwest::Client,
    fallback_session: String,
}

impl OpencodeCompletionModel {
    // Headers belong to the call, not to the pooled client: two sessions may
    // concurrently use the same model. Cloned HTTP clients reuse the pool.
    pub(crate) fn for_session(&self, session: Option<&str>) -> crate::Result<AnyCompletionModel> {
        let mut headers = request_headers(
            session
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.fallback_session),
        )?;
        let base = format!("{}/{}", self.base_url, self.protocol.base_path());
        let api_key = match self.protocol {
            // rig puts Gemini's key in the URI. Use header auth for OpenCode
            // and leave that query value empty so URLs cannot expose the key.
            Protocol::Gemini => {
                headers.insert(
                    "x-goog-api-key",
                    HeaderValue::from_str(&self.api_key).map_err(|e| {
                        LlmError::Config(format!("invalid OpenCode API key header: {e}"))
                    })?,
                );
                ""
            }
            _ => self.api_key.as_str(),
        };
        macro_rules! client {
            ($provider:ident) => {
                $provider::Client::builder()
                    .api_key(api_key)
                    .base_url(&base)
                    .http_client(self.http_client.clone())
                    .http_headers(headers)
                    .build()
                    .map_err(|e| LlmError::Config(format!("build OpenCode client: {e}")))?
            };
        }
        Ok(match self.protocol {
            Protocol::ChatCompletions => AnyCompletionModel::OpenAI(
                client!(openai)
                    .completions_api()
                    .completion_model(&self.model),
            ),
            Protocol::Responses => {
                AnyCompletionModel::OpenAIResponses(client!(openai).completion_model(&self.model))
            }
            Protocol::Messages => AnyCompletionModel::Anthropic(
                client!(anthropic)
                    .completion_model(&self.model)
                    .with_prompt_caching(),
            ),
            Protocol::Gemini => {
                AnyCompletionModel::Gemini(client!(gemini).completion_model(&self.model))
            }
        })
    }

    pub(crate) fn accepts_audio_content(&self) -> bool {
        self.protocol == Protocol::Gemini
    }

    pub(crate) fn accepts_pdf_document(&self) -> bool {
        self.protocol != Protocol::ChatCompletions
    }

    pub(crate) fn image_tokens(&self, width: u32, height: u32) -> u64 {
        match self.protocol {
            Protocol::Responses => crate::openai_image_tokens(width, height),
            Protocol::Messages => crate::anthropic_image_tokens(width, height),
            Protocol::Gemini => crate::gemini_image_tokens(width, height),
            Protocol::ChatCompletions => crate::provider_image_tokens(width, height),
        }
    }
}

fn request_headers(session: &str) -> crate::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(CLIENT_USER_AGENT));
    headers.insert(
        SESSION_HEADER,
        HeaderValue::from_str(session)
            .map_err(|e| LlmError::Config(format!("invalid OpenCode session header: {e}")))?,
    );
    Ok(headers)
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use baybo_model::{ChatMessage, ContentBlock};
    use futures::StreamExt;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::{ChatRequest, LlmProviderRegistry, StreamEvent, ToolDefinitionForLlm};

    struct CapturedRequest {
        path: String,
        headers: HashMap<String, String>,
        body: Value,
    }

    async fn endpoint(
        responses: Vec<(u16, &'static str, String)>,
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<CapturedRequest>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for (status, content_type, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                let header_end = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let mut lines = head.lines();
                let path = lines
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_string();
                let headers: HashMap<_, _> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let len: usize = headers
                    .get("content-length")
                    .map(|n| n.parse().unwrap())
                    .unwrap_or(0);
                while bytes.len() < header_end + len {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                let _ = tx.send(CapturedRequest {
                    path,
                    headers,
                    body: if len == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap()
                    },
                });
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (origin, rx)
    }

    fn config(
        plan: OpencodeProviderFactory,
        model: &str,
        base: Option<String>,
    ) -> LlmProviderConfig {
        LlmProviderConfig {
            provider: plan.provider_name().into(),
            api_key: Some("test-opencode-key".into()),
            base_url: base,
            model: model.into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: Some("high".into()),
            vault: None,
            proxy: None,
        }
    }

    fn request(session: &str) -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage::user(vec![ContentBlock::Text(
                "Read the project file".into(),
            )])],
            prompt_cache_key: Some(session.into()),
            tools: vec![ToolDefinitionForLlm {
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            }],
            ..Default::default()
        }
    }

    fn chat_response() -> Value {
        json!({"id":"chat_1","object":"chat.completion","created":1,"model":"glm-5.3","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})
    }

    fn responses_response(output: Value) -> Value {
        json!({"id":"resp_1","object":"response","created_at":1,"status":"completed","model":"gpt-5.6-luna","output":output,"usage":{"input_tokens":10,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":12}})
    }

    fn text_output() -> Value {
        json!([{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}])
    }

    #[test]
    fn registry_exposes_both_plans_with_separate_credentials() {
        let registry = LlmProviderRegistry::with_default_providers();
        for plan in [OpencodeProviderFactory::Zen, OpencodeProviderFactory::Go] {
            let factory = registry.factory_for(plan.provider_name()).unwrap();
            assert_eq!(factory.default_base_url(), Some(plan.base_url()));
            assert_eq!(factory.default_api_key_env(), Some(plan.api_key_env()));
            let mut cfg = config(plan, "kimi-k2.5", None);
            cfg.context_window = Some(128_000);
            cfg.supports_vision = Some(true);
            let client = registry.build_client(&cfg).unwrap();
            assert_eq!(client.model_info().provider, plan.provider_name());
            assert_eq!(client.model_info().context_window, 128_000);
            assert!(client.model_info().supports_vision);
            for key in [None, Some(String::new())] {
                cfg.api_key = key;
                assert!(matches!(
                    registry.build_client(&cfg),
                    Err(LlmError::Config(_))
                ));
            }
        }
    }

    #[tokio::test]
    async fn catalogs_use_the_selected_plan_and_exclude_non_chat_models() {
        let payload = json!({"data":[{"id":"kimi-k2.5"},{"id":"jev-1.13"},{"id":"gpt-5.6-luna"},{"id":"kimi-k2.5"}]}).to_string();
        let (origin, mut captured) = endpoint(vec![
            (200, "application/json", payload.clone()),
            (200, "application/json", payload),
        ])
        .await;
        let registry = LlmProviderRegistry::with_default_providers();
        for (plan, suffix) in [
            (OpencodeProviderFactory::Zen, ""),
            (OpencodeProviderFactory::Go, "/go"),
        ] {
            let models = registry
                .list_live_models(&config(plan, "unused", Some(format!("{origin}{suffix}/"))))
                .await
                .unwrap();
            assert_eq!(
                models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                ["gpt-5.6-luna", "kimi-k2.5"]
            );
            let req = captured.recv().await.unwrap();
            assert_eq!(req.path, format!("{suffix}/v1/models"));
            assert_eq!(req.headers["authorization"], "Bearer test-opencode-key");
            assert_eq!(req.headers["user-agent"], CLIENT_USER_AGENT);
        }
    }

    #[tokio::test]
    async fn native_protocols_send_auth_effort_tools_and_session_headers() {
        let messages = json!({"type":"message","id":"msg_1","model":"claude-haiku-5-5","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":6,"cache_read_input_tokens":3,"cache_creation_input_tokens":1,"output_tokens":2}});
        let gemini = json!({"responseId":"gemini_1","candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12,"cachedContentTokenCount":3}});
        let cases = [
            (
                OpencodeProviderFactory::Go,
                "glm-5.3",
                "/openai/v1/chat/completions",
                chat_response(),
                "reasoning_effort",
                "authorization",
            ),
            (
                OpencodeProviderFactory::Zen,
                "qwen3.8-max",
                "/openai/v1/chat/completions",
                chat_response(),
                "reasoning_effort",
                "authorization",
            ),
            (
                OpencodeProviderFactory::Go,
                "minimax-m3",
                "/anthropic/v1/messages",
                messages.clone(),
                "output_config",
                "x-api-key",
            ),
            (
                OpencodeProviderFactory::Go,
                "qwen3.8-max",
                "/anthropic/v1/messages",
                messages,
                "output_config",
                "x-api-key",
            ),
            (
                OpencodeProviderFactory::Go,
                "gpt-5.6-luna",
                "/openai/v1/responses",
                responses_response(text_output()),
                "reasoning",
                "authorization",
            ),
            (
                OpencodeProviderFactory::Zen,
                "gemini-3.1-pro",
                "/google/v1beta/models/gemini-3.1-pro:generateContent?key=",
                gemini,
                "generationConfig",
                "x-goog-api-key",
            ),
        ];
        for (plan, model, path, response, effort_field, auth) in cases {
            let (origin, mut captured) =
                endpoint(vec![(200, "application/json", response.to_string())]).await;
            let client = LlmProviderRegistry::with_default_providers()
                .build_client(&config(plan, model, Some(origin)))
                .unwrap();
            let result = client
                .chat(&request("session-1"))
                .await
                .unwrap_or_else(|error| panic!("{plan:?} {model}: {error}"));
            assert_eq!(result.content, "done", "{model}");
            assert_eq!(result.usage.input_tokens, 10, "{model}");
            assert_eq!(result.usage.output_tokens, 2, "{model}");
            let req = captured.recv().await.unwrap();
            assert_eq!(req.path, path);
            assert!(!req.path.contains("test-opencode-key"));
            assert!(req.headers[auth].ends_with("test-opencode-key"));
            assert_eq!(req.headers[SESSION_HEADER], "session-1");
            assert_eq!(req.headers["user-agent"], CLIENT_USER_AGENT);
            assert!(!req.body[effort_field].is_null(), "{model}: {}", req.body);
            assert!(!req.body["tools"].is_null());
        }
    }

    #[tokio::test]
    async fn responses_preserve_tool_identity_across_durable_transcript_round_trip() {
        let output = json!([{"type":"function_call","id":"fc_item","call_id":"call_tool","name":"read_file","arguments":"{}","status":"completed"}]);
        let (origin, mut captured) = endpoint(vec![
            (
                200,
                "application/json",
                responses_response(output).to_string(),
            ),
            (
                200,
                "application/json",
                responses_response(text_output()).to_string(),
            ),
        ])
        .await;
        let client = LlmProviderRegistry::with_default_providers()
            .build_client(&config(
                OpencodeProviderFactory::Go,
                "gpt-5.6-luna",
                Some(origin),
            ))
            .unwrap();
        let mut req = request("session-tools");
        let first = client.chat(&req).await.unwrap();
        let call = first.tool_calls.into_iter().next().unwrap();
        req.messages
            .push(ChatMessage::assistant(vec![ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name,
                input: call.arguments,
                signature: call.signature,
            }]));
        req.messages
            .push(ChatMessage::tool_result(call.id, "file contents".into()));
        let restored: ChatRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(client.chat(&restored).await.unwrap().content, "done");
        captured.recv().await.unwrap();
        let wire = captured.recv().await.unwrap();
        let items = wire.body["input"].as_array().unwrap();
        let call = items.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(call["id"], "fc_item");
        assert_eq!(call["call_id"], "call_tool");
        let result = items
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(result["call_id"], "call_tool");
        assert_eq!(result["output"], "file contents");
        assert_eq!(wire.body["store"], false);
        assert_eq!(wire.body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[tokio::test]
    async fn compatible_chat_preserves_reasoning_on_tool_followups() {
        let mut first = chat_response();
        first["choices"][0]["message"] = json!({
            "role":"assistant", "content":null, "reasoning_content":"Read the project before editing",
            "tool_calls":[{"id":"call_read","type":"function","function":{"name":"read_file","arguments":"{}"}}]
        });
        first["choices"][0]["finish_reason"] = "tool_calls".into();
        let (origin, mut captured) = endpoint(vec![
            (200, "application/json", first.to_string()),
            (200, "application/json", chat_response().to_string()),
        ])
        .await;
        let client = LlmProviderRegistry::with_default_providers()
            .build_client(&config(
                OpencodeProviderFactory::Go,
                "kimi-k3",
                Some(origin),
            ))
            .unwrap();
        let mut req = request("session-thinking");
        let response = client.chat(&req).await.unwrap();
        let call = response.tool_calls.into_iter().next().unwrap();
        let mut blocks = response.content_blocks;
        blocks.push(ContentBlock::ToolUse {
            id: call.id.clone(),
            name: call.name,
            input: call.arguments,
            signature: call.signature,
        });
        req.messages.push(ChatMessage::assistant(blocks));
        req.messages
            .push(ChatMessage::tool_result(call.id, "file contents".into()));
        assert_eq!(client.chat(&req).await.unwrap().content, "done");
        captured.recv().await.unwrap();
        let wire = captured.recv().await.unwrap();
        let assistant = wire.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert_eq!(
            assistant["reasoning_content"],
            "Read the project before editing"
        );
        assert_eq!(assistant["tool_calls"][0]["id"], "call_read");
    }

    #[tokio::test]
    async fn streaming_sessions_do_not_share_affinity_headers() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
            "data: [DONE]\n\n"
        ).to_string();
        let (origin, mut captured) = endpoint(vec![
            (200, "text/event-stream", sse.clone()),
            (200, "text/event-stream", sse),
        ])
        .await;
        let client = LlmProviderRegistry::with_default_providers()
            .build_client(&config(
                OpencodeProviderFactory::Go,
                "kimi-k2.5",
                Some(origin),
            ))
            .unwrap();
        let a = request("session-a");
        let b = request("session-b");
        let (a, b) = tokio::join!(client.chat_stream(&a), client.chat_stream(&b));
        for mut stream in [a.unwrap(), b.unwrap()] {
            let mut text = String::new();
            let mut tokens = 0;
            while let Some(event) = stream.next().await {
                match event.unwrap() {
                    StreamEvent::Text(t) => text.push_str(&t),
                    StreamEvent::Usage(usage) => tokens = usage.input_tokens,
                    _ => {}
                }
            }
            assert_eq!(text, "done");
            assert_eq!(tokens, 10);
        }
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let req = captured.recv().await.unwrap();
            assert_eq!(req.path, "/openai/v1/chat/completions");
            assert_eq!(req.body["stream"], true);
            sessions.push(req.headers[SESSION_HEADER].clone());
        }
        sessions.sort();
        assert_eq!(sessions, ["session-a", "session-b"]);
    }

    #[tokio::test]
    async fn responses_stream_retains_tool_ids_and_usage() {
        let item = json!({"type":"function_call","id":"fc_item","call_id":"call_tool","name":"read_file","arguments":"{}","status":"completed"});
        let events = [
            json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":item}),
            json!({"type":"response.completed","sequence_number":2,"response":responses_response(json!([item]))}),
        ];
        let sse = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let (origin, mut captured) = endpoint(vec![
            (200, "text/event-stream", sse),
            (
                200,
                "application/json",
                responses_response(text_output()).to_string(),
            ),
        ])
        .await;
        let client = LlmProviderRegistry::with_default_providers()
            .build_client(&config(
                OpencodeProviderFactory::Go,
                "gpt-5.6-luna",
                Some(origin),
            ))
            .unwrap();
        let mut req = request("session-stream-tools");
        let mut stream = client.chat_stream(&req).await.unwrap();
        let mut calls = Vec::new();
        let mut tokens = 0;
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                StreamEvent::ToolCall(call) => calls.push(call),
                StreamEvent::Usage(usage) => tokens = usage.input_tokens,
                _ => {}
            }
        }
        assert_eq!(tokens, 10);
        assert_eq!(calls.len(), 1);
        let call = calls.remove(0);
        req.messages
            .push(ChatMessage::assistant(vec![ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name,
                input: call.arguments,
                signature: call.signature,
            }]));
        req.messages
            .push(ChatMessage::tool_result(call.id, "file contents".into()));
        assert_eq!(client.chat(&req).await.unwrap().content, "done");
        captured.recv().await.unwrap();
        let wire = captured.recv().await.unwrap();
        let items = wire.body["input"].as_array().unwrap();
        let call = items.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(call["id"], "fc_item");
        assert_eq!(call["call_id"], "call_tool");
    }

    #[tokio::test]
    async fn catalog_errors_keep_auth_and_rate_limit_classification() {
        let (origin, _) = endpoint(vec![(401, "application/json", "{}".into())]).await;
        let err = OpencodeProviderFactory::Go
            .live_models(&config(OpencodeProviderFactory::Go, "unused", Some(origin)))
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Auth(_)));
        let (origin, _) = endpoint(vec![(429, "application/json", "{}".into())]).await;
        let err = OpencodeProviderFactory::Zen
            .live_models(&config(
                OpencodeProviderFactory::Zen,
                "unused",
                Some(origin),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }
}
