use std::collections::HashMap;

use rig::client::CompletionClient;
use rig::completion::{AssistantContent, CompletionRequest, CompletionResponse};
use rig::message::{Message, ToolCall, UserContent};
use serde::Deserialize;

use crate::providers::rig_providers::rig_provider_factory;
use crate::registry::{LiveModelInfo, LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelPricing};

// Host root only — rig's gemini client appends the versioned path
// (`/v1beta/models/{model}:generateContent`) on every request, so the
// base URL must NOT include `/v1beta` or we end up with `/v1beta/v1beta/…`.
pub(crate) const GEMINI_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

rig_provider_factory!(
    GeminiProviderFactory,
    "gemini",
    gemini,
    Gemini,
    effort = Gemini,
    priced = crate::providers::catalog::GEMINI_FLAT_DEFAULT_PRICING,
    live_models = gemini_live_models,
    api_key_env = "GEMINI_API_KEY",
    base_url = GEMINI_DEFAULT_BASE_URL
);

/// Length of a minted Gemini tool-call id. Mistral is the strictest
/// provider a conversation can move to: it rejects any tool-call id that is
/// not exactly nine ASCII alphanumerics, and nine alphanumerics also satisfy
/// Anthropic's `^[a-zA-Z0-9_-]{1,64}$` and OpenAI's 40-character cap.
const TOOL_CALL_ID_LEN: usize = 9;
const TOOL_CALL_ID_ALPHABET: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

// Gemini has no call ids, and rig fills the id with the function name, so
// two calls to one tool would share an id everywhere else in baybo.
fn mint_tool_call_id() -> String {
    let base = TOOL_CALL_ID_ALPHABET.len() as u128;
    let mut bits = uuid::Uuid::new_v4().as_u128();
    let mut id = String::with_capacity(TOOL_CALL_ID_LEN);
    for _ in 0..TOOL_CALL_ID_LEN {
        id.push(char::from(TOOL_CALL_ID_ALPHABET[(bits % base) as usize]));
        bits /= base;
    }
    id
}

/// Give a tool call Gemini returned an id unique within the transcript.
pub(crate) fn assign_tool_call_id(call: &mut ToolCall) {
    call.id = mint_tool_call_id();
}

/// [`assign_tool_call_id`] over every tool call of a unary response.
pub(crate) fn assign_response_tool_call_ids(
    mut response: CompletionResponse<()>,
) -> CompletionResponse<()> {
    for part in response.choice.iter_mut() {
        if let AssistantContent::ToolCall(call) = part {
            assign_tool_call_id(call);
        }
    }
    response
}

/// rig sends a tool result's id as Gemini's `functionResponse.name`, which
/// must name the function it answers. Rewrite each result id to the name of
/// the nearest preceding call carrying that id; a result with no such call
/// is left as it is.
pub(crate) fn prepare_request(mut request: CompletionRequest) -> CompletionRequest {
    let mut call_names: HashMap<String, String> = HashMap::new();
    for message in request.chat_history.iter_mut() {
        match message {
            Message::Assistant { content, .. } => {
                for part in content.iter() {
                    if let AssistantContent::ToolCall(call) = part {
                        call_names.insert(call.id.clone(), call.function.name.clone());
                    }
                }
            }
            Message::User { content } => {
                for part in content.iter_mut() {
                    if let UserContent::ToolResult(result) = part
                        && let Some(name) = call_names.get(&result.id)
                    {
                        result.id.clone_from(name);
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    request
}

/// Live discovery against Gemini's catalog endpoint. Filters to models
/// that can `generateContent` (skipping embedding/tuning-only entries)
/// and strips the API's `models/` id prefix so the slug matches the
/// shape `CompletionModel` expects.
async fn gemini_live_models(config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
    let api_key = config.api_key.as_deref().ok_or_else(|| {
        crate::LlmError::Config("Gemini live discovery requires an API key".into())
    })?;
    let base = config
        .base_url
        .as_deref()
        .unwrap_or(GEMINI_DEFAULT_BASE_URL)
        .trim_end_matches('/');
    // Tolerate stale configs whose base already includes the version
    // segment — the canonical base is the host root.
    let host_root = base.strip_suffix("/v1beta").unwrap_or(base);
    // Gemini API keys are alphanumeric — no encoding needed — but we
    // still send the key as a header where supported, so it doesn't
    // land in URL access logs. Header form is documented at
    // <https://ai.google.dev/gemini-api/docs/api-key>.
    let url = format!("{host_root}/v1beta/models?pageSize=1000");
    let resp = crate::proxied_client(config.proxy.as_ref())?
        .get(&url)
        .header("x-goog-api-key", api_key)
        .send()
        .await
        .map_err(|e| crate::reqwest_to_error(e, "gemini GET /models"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::status_to_error(
            status.as_u16(),
            format!("gemini GET /models returned {status}: {body}"),
        ));
    }
    let payload: ModelListResponse = resp
        .json()
        .await
        .map_err(|e| crate::LlmError::Decode(format!("gemini /models: parse response: {e}")))?;
    Ok(payload
        .models
        .into_iter()
        .filter(|m| {
            m.supported_generation_methods
                .iter()
                .flatten()
                .any(|s| s == "generateContent")
        })
        .map(|m| {
            // The API returns ids prefixed with "models/"; the
            // CompletionModel constructor expects the bare slug.
            let id = m
                .name
                .strip_prefix("models/")
                .unwrap_or(&m.name)
                .to_string();
            let extras = serde_json::json!({
                "version": m.version,
                "supported_generation_methods": m.supported_generation_methods,
            });
            LiveModelInfo {
                id,
                display_name: m.display_name,
                description: m.description,
                context_window: m.input_token_limit.map(|n| n as usize),
                extras,
                ..Default::default()
            }
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    #[serde(default)]
    models: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default, rename = "inputTokenLimit")]
    input_token_limit: Option<i64>,
    #[serde(default, rename = "supportedGenerationMethods")]
    supported_generation_methods: Option<Vec<String>>,
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashSet;

    use baybo_model::{ChatMessage, ContentBlock};
    use futures::StreamExt;
    use serde_json::{Value, json};

    use super::*;
    use crate::mock_http::endpoint;
    use crate::{
        ChatRequest, LlmProviderRegistry, StreamEvent, ToolCallInfo, ToolDefinitionForLlm,
    };

    #[test]
    fn test_provider_name() {
        let factory = GeminiProviderFactory;
        assert_eq!(factory.provider_name(), "gemini");
    }

    fn config(provider: &str, model: &str, origin: String) -> LlmProviderConfig {
        LlmProviderConfig {
            provider: provider.into(),
            api_key: Some("test-key".into()),
            base_url: Some(origin),
            model: model.into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
            proxy: None,
        }
    }

    fn gemini_client(origin: String) -> crate::LlmClient {
        LlmProviderRegistry::with_default_providers()
            .build_client(&config("gemini", "gemini-test", origin))
            .unwrap()
    }

    pub(crate) fn search_tools_request() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage::user(vec![ContentBlock::Text(
                "Look two things up".into(),
            )])],
            tools: vec![ToolDefinitionForLlm {
                name: "web_search".into(),
                description: "Search the web".into(),
                parameters_schema: json!({"type":"object","properties":{"q":{"type":"string"}}}),
            }],
            ..Default::default()
        }
    }

    /// One model turn calling `web_search` twice; the first call is signed.
    pub(crate) fn repeated_call_parts() -> Value {
        json!([
            {"functionCall":{"name":"web_search","args":{"q":"alpha"}},"thoughtSignature":"sig-a"},
            {"functionCall":{"name":"web_search","args":{"q":"beta"}}}
        ])
    }

    pub(crate) fn repeated_call_sse() -> String {
        [
            json!({"responseId":"r1","candidates":[{"content":{"role":"model","parts":repeated_call_parts()}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6}}),
            json!({"responseId":"r1","candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6}}),
        ]
        .iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect()
    }

    pub(crate) fn repeated_call_body() -> String {
        json!({"responseId":"r1","candidates":[{"content":{"role":"model","parts":repeated_call_parts()},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6}}).to_string()
    }

    pub(crate) async fn collect_calls(mut stream: crate::LlmStream) -> Vec<ToolCallInfo> {
        let mut calls = Vec::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::ToolCall(call) = event.unwrap() {
                calls.push(call);
            }
        }
        calls
    }

    /// Two `web_search` calls come back with distinct ids that every
    /// provider accepts, each keeping its own arguments and signature.
    pub(crate) fn assert_distinct_portable_ids(calls: &[ToolCallInfo]) {
        assert_eq!(calls.len(), 2);
        let ids: HashSet<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "repeated calls share an id: {calls:?}");
        for call in calls {
            assert_eq!(call.name, "web_search");
            assert_ne!(call.id, call.name);
            assert_eq!(call.id.len(), TOOL_CALL_ID_LEN);
            assert!(
                call.id.chars().all(|c| c.is_ascii_alphanumeric()),
                "{}",
                call.id
            );
        }
        assert_eq!(calls[0].arguments, json!({"q":"alpha"}));
        assert_eq!(calls[1].arguments, json!({"q":"beta"}));
        assert_eq!(calls[0].signature.as_deref(), Some("sig-a"));
        assert_eq!(calls[1].signature, None);
    }

    fn tool_use(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: json!({}),
            signature: None,
        }
    }

    fn result(id: &str) -> ChatMessage {
        ChatMessage::tool_result_with_meta(id.into(), format!("output of {id}"), None)
    }

    /// A history with Gemini-minted ids, an MCP-namespaced tool, an
    /// Anthropic-style id, and an unpaired result.
    pub(crate) fn mixed_history_request() -> ChatRequest {
        let mut request = search_tools_request();
        request.messages.extend([
            ChatMessage::assistant(vec![
                tool_use("A1b2C3d4E", "web_search"),
                tool_use("F5g6H7i8J", "web_search"),
                tool_use("K9l0M1n2O", "browser/navigate_page"),
            ]),
            result("A1b2C3d4E"),
            result("F5g6H7i8J"),
            result("K9l0M1n2O"),
            ChatMessage::assistant(vec![tool_use("toolu_01Xyz", "read_file")]),
            result("toolu_01Xyz"),
        ]);
        request
    }

    /// `(functionCall names, functionResponse names)` of a captured body.
    pub(crate) fn function_names(body: &Value) -> (Vec<String>, Vec<String>) {
        let mut calls = Vec::new();
        let mut responses = Vec::new();
        for part in body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["parts"].as_array().unwrap())
        {
            if let Some(name) = part["functionCall"]["name"].as_str() {
                calls.push(name.to_string());
            }
            if let Some(name) = part["functionResponse"]["name"].as_str() {
                responses.push(name.to_string());
            }
        }
        (calls, responses)
    }

    pub(crate) fn assert_responses_name_their_calls(body: &Value) {
        let (calls, responses) = function_names(body);
        let expected = [
            "web_search",
            "web_search",
            "browser__navigate_page",
            "read_file",
        ];
        assert_eq!(calls, expected);
        assert_eq!(responses, expected);
    }

    #[tokio::test]
    async fn native_stream_gives_repeated_tool_calls_distinct_ids() {
        let (origin, _captured) =
            endpoint(vec![(200, "text/event-stream", repeated_call_sse())]).await;
        let stream = gemini_client(origin)
            .chat_stream(&search_tools_request())
            .await
            .unwrap();
        assert_distinct_portable_ids(&collect_calls(stream).await);
    }

    #[tokio::test]
    async fn native_completion_gives_repeated_tool_calls_distinct_ids() {
        let (origin, _captured) =
            endpoint(vec![(200, "application/json", repeated_call_body())]).await;
        let response = gemini_client(origin)
            .chat(&search_tools_request())
            .await
            .unwrap();
        assert_distinct_portable_ids(&response.tool_calls);
    }

    #[tokio::test]
    async fn native_function_responses_carry_the_called_function_name() {
        let (origin, mut captured) = endpoint(vec![
            (200, "text/event-stream", repeated_call_sse()),
            (200, "application/json", repeated_call_body()),
        ])
        .await;
        let client = gemini_client(origin);
        let request = mixed_history_request();
        collect_calls(client.chat_stream(&request).await.unwrap()).await;
        client.chat(&request).await.unwrap();
        for _ in 0..2 {
            assert_responses_name_their_calls(&captured.recv().await.unwrap().body);
        }
    }

    #[test]
    fn unpaired_results_keep_their_id() {
        let request = CompletionRequest {
            model: None,
            preamble: None,
            chat_history: rig::OneOrMany::many(vec![
                Message::Assistant {
                    id: None,
                    content: rig::OneOrMany::one(AssistantContent::ToolCall(ToolCall::new(
                        "call_a".into(),
                        rig::message::ToolFunction::new("read_file".into(), json!({})),
                    ))),
                },
                Message::User {
                    content: rig::OneOrMany::many(vec![
                        UserContent::tool_result(
                            "call_a",
                            rig::OneOrMany::one("a".to_string().into()),
                        ),
                        UserContent::tool_result(
                            "ghost",
                            rig::OneOrMany::one("b".to_string().into()),
                        ),
                    ])
                    .unwrap(),
                },
            ])
            .unwrap(),
            documents: vec![],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };
        let prepared = prepare_request(request);
        let ids: Vec<String> = prepared
            .chat_history
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::ToolResult(r) => Some(r.id.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            })
            .collect();
        assert_eq!(ids, ["read_file", "ghost"]);
    }

    /// History a Gemini turn produced, replayed to an Anthropic-wire model:
    /// every `tool_use` id is unique and in Anthropic's id format.
    #[tokio::test]
    async fn gemini_history_replays_to_anthropic_with_unique_ids() {
        let (origin, _captured) =
            endpoint(vec![(200, "text/event-stream", repeated_call_sse())]).await;
        let calls = collect_calls(
            gemini_client(origin)
                .chat_stream(&search_tools_request())
                .await
                .unwrap(),
        )
        .await;
        let mut request = search_tools_request();
        request.messages.push(ChatMessage::assistant(
            calls.iter().map(|c| tool_use(&c.id, &c.name)).collect(),
        ));
        request.messages.extend(calls.iter().map(|c| result(&c.id)));

        let reply = json!({"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}});
        let (origin, mut captured) =
            endpoint(vec![(200, "application/json", reply.to_string())]).await;
        LlmProviderRegistry::with_default_providers()
            .build_client(&config("anthropic", "claude-sonnet-4-5", origin))
            .unwrap()
            .chat(&request)
            .await
            .unwrap();
        let body = captured.recv().await.unwrap().body;
        let blocks: Vec<&Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .collect();
        let use_ids: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        let result_ids: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| b["tool_use_id"].as_str().unwrap())
            .collect();
        assert_eq!(use_ids.len(), 2);
        assert_eq!(use_ids.iter().collect::<HashSet<_>>().len(), 2);
        assert_eq!(result_ids, use_ids);
        let expected: Vec<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(use_ids, expected, "Anthropic requests carry ids unchanged");
        for id in use_ids {
            assert!(
                id.len() <= 64
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{id}"
            );
        }
    }
}
