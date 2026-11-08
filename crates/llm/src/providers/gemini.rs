use std::collections::HashMap;

use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::{AssistantContent, CompletionRequest, CompletionResponse};
use rig::message::{Message, Reasoning, ReasoningContent, ToolCall, UserContent};
use rig::streaming::StreamedAssistantContent;
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

/// Marks a thought signature baybo stamped with the model that produced it:
/// `<prefix><model>|<signature>`. Gemini signatures are base64, so they never
/// contain the separator, and model ids never do either.
const SIGNATURE_STAMP_PREFIX: &str = "baybo:gemini-sig:v1:";
const SIGNATURE_STAMP_SEPARATOR: char = '|';

/// Gemini's documented stand-in for a thought signature it did not issue.
/// A signature from another model is rejected as invalid, and a function
/// call in the current turn with no signature is rejected as missing one;
/// this value is accepted in both places.
pub(crate) const THOUGHT_SIGNATURE_PLACEHOLDER: &str = "skip_thought_signature_validator";

fn stamp_signature(signature: &mut Option<String>, model: &str) {
    if let Some(raw) = signature.take() {
        *signature = Some(format!(
            "{SIGNATURE_STAMP_PREFIX}{model}{SIGNATURE_STAMP_SEPARATOR}{raw}"
        ));
    }
}

/// The raw signature, when `stamped` was issued by `model`. Unstamped
/// signatures predate provenance and count as foreign.
fn own_signature<'a>(stamped: &'a str, model: &str) -> Option<&'a str> {
    let (issuer, raw) = stamped
        .strip_prefix(SIGNATURE_STAMP_PREFIX)?
        .split_once(SIGNATURE_STAMP_SEPARATOR)?;
    (issuer == model).then_some(raw)
}

fn stamp_reasoning(reasoning: &mut Reasoning, model: &str) {
    for content in &mut reasoning.content {
        if let ReasoningContent::Text { signature, .. } = content {
            stamp_signature(signature, model);
        }
    }
}

/// Adopt a tool call `model` returned: give it an id unique within the
/// transcript and stamp its signature with `model`.
pub(crate) fn adopt_tool_call(call: &mut ToolCall, model: &str) {
    call.id = mint_tool_call_id();
    stamp_signature(&mut call.signature, model);
}

/// [`adopt_tool_call`] and [`stamp_reasoning`] over one streamed event.
pub(crate) fn adopt_stream_event<R>(event: &mut StreamedAssistantContent<R>, model: &str) {
    match event {
        StreamedAssistantContent::ToolCall { tool_call, .. } => adopt_tool_call(tool_call, model),
        StreamedAssistantContent::Reasoning(reasoning) => stamp_reasoning(reasoning, model),
        _ => {}
    }
}

/// [`adopt_stream_event`] over every part of a unary response.
pub(crate) fn adopt_response(
    mut response: CompletionResponse<()>,
    model: &str,
) -> CompletionResponse<()> {
    for part in response.choice.iter_mut() {
        match part {
            AssistantContent::ToolCall(call) => adopt_tool_call(call, model),
            AssistantContent::Reasoning(reasoning) => stamp_reasoning(reasoning, model),
            _ => {}
        }
    }
    response
}

/// Replay only `model`'s own signatures. A tool call signed by anyone else
/// carries the placeholder instead, and a model turn whose calls end up with
/// no signature at all gets the placeholder on its first call, since Gemini
/// rejects an unsigned call in the current turn. Foreign thinking is dropped
/// from the request (its signature cleared when it is the turn's only part).
fn replay_signatures(content: &mut OneOrMany<AssistantContent>, model: &str) {
    let mut signed = false;
    for part in content.iter_mut() {
        match part {
            AssistantContent::ToolCall(call) => {
                call.signature = call.signature.as_deref().map(|stamped| {
                    own_signature(stamped, model)
                        .unwrap_or(THOUGHT_SIGNATURE_PLACEHOLDER)
                        .to_string()
                });
                signed |= call.signature.is_some();
            }
            AssistantContent::Reasoning(reasoning) => {
                for block in &mut reasoning.content {
                    if let ReasoningContent::Text { signature, .. } = block {
                        *signature = signature
                            .as_deref()
                            .and_then(|stamped| own_signature(stamped, model))
                            .map(str::to_string);
                    }
                }
            }
            _ => {}
        }
    }
    if !signed
        && let Some(AssistantContent::ToolCall(call)) = content
            .iter_mut()
            .find(|part| matches!(part, AssistantContent::ToolCall(_)))
    {
        call.signature = Some(THOUGHT_SIGNATURE_PLACEHOLDER.to_string());
    }
}

/// Which reasoning parts of a stored turn to keep: thinking that carried a
/// signature `model` did not issue is foreign and goes.
fn drop_foreign_thinking(content: &mut OneOrMany<AssistantContent>, model: &str) {
    let foreign = |part: &AssistantContent| match part {
        AssistantContent::Reasoning(reasoning) => reasoning
            .first_signature()
            .is_some_and(|stamped| own_signature(stamped, model).is_none()),
        _ => false,
    };
    if !content.iter().any(foreign) {
        return;
    }
    let kept: Vec<AssistantContent> = content.iter().filter(|p| !foreign(p)).cloned().collect();
    if let Ok(kept) = OneOrMany::many(kept) {
        *content = kept;
    }
}

/// Shape a request for Gemini `model`:
/// - rig sends a tool result's id as `functionResponse.name`, which must name
///   the function it answers, so each result id becomes the name of the
///   nearest preceding call carrying that id (an unpaired result keeps it);
/// - thought signatures are replayed only to the model that issued them
///   (see [`replay_signatures`]).
pub(crate) fn prepare_request(mut request: CompletionRequest, model: &str) -> CompletionRequest {
    let mut call_names: HashMap<String, String> = HashMap::new();
    for message in request.chat_history.iter_mut() {
        match message {
            Message::Assistant { content, .. } => {
                for part in content.iter() {
                    if let AssistantContent::ToolCall(call) = part {
                        call_names.insert(call.id.clone(), call.function.name.clone());
                    }
                }
                drop_foreign_thinking(content, model);
                replay_signatures(content, model);
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

    /// The raw signature `model` issued, read back out of a stored stamp.
    pub(crate) fn issued_by<'a>(stamped: Option<&'a str>, model: &str) -> Option<&'a str> {
        stamped.and_then(|s| own_signature(s, model))
    }

    /// Two `web_search` calls come back with distinct ids that every
    /// provider accepts, each keeping its own arguments and signature,
    /// stamped as issued by `model`.
    pub(crate) fn assert_distinct_portable_ids(calls: &[ToolCallInfo], model: &str) {
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
        assert_eq!(
            issued_by(calls[0].signature.as_deref(), model),
            Some("sig-a")
        );
        assert_eq!(
            issued_by(calls[0].signature.as_deref(), "another-model"),
            None
        );
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

    /// Count of `key` parts in one Gemini content.
    fn parts_with(content: &Value, key: &str) -> usize {
        content["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part.get(key).is_some())
            .count()
    }

    /// Gemini's own check on a request body: a model content with N
    /// `functionCall` parts must be followed directly by one user content
    /// holding exactly N `functionResponse` parts, and no response may sit
    /// anywhere else. The real API answers a violation with
    /// `400 INVALID_ARGUMENT`; the mock endpoint answers 200 to anything, so
    /// every test that sends tool history checks the body with this.
    pub(crate) fn assert_gemini_accepts_function_turns(body: &Value) {
        let contents = body["contents"].as_array().unwrap();
        for (i, content) in contents.iter().enumerate() {
            let calls = parts_with(content, "functionCall");
            let responses = parts_with(content, "functionResponse");
            if calls > 0 {
                assert_eq!(content["role"], "model", "content {i}: {body}");
                let next = contents
                    .get(i + 1)
                    .unwrap_or_else(|| panic!("content {i}: calls with no response turn: {body}"));
                assert_eq!(next["role"], "user", "content {}: {body}", i + 1);
                assert_eq!(
                    parts_with(next, "functionResponse"),
                    calls,
                    "content {} must answer all {calls} function calls of content {i}: {body}",
                    i + 1
                );
            }
            if responses > 0 {
                let answered = i
                    .checked_sub(1)
                    .map(|prev| parts_with(&contents[prev], "functionCall"))
                    .unwrap_or(0);
                assert_eq!(
                    responses, answered,
                    "content {i}: {responses} function responses answer {answered} calls: {body}"
                );
            }
        }
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
        assert_distinct_portable_ids(&collect_calls(stream).await, "gemini-test");
    }

    #[tokio::test]
    async fn native_completion_gives_repeated_tool_calls_distinct_ids() {
        let (origin, _captured) =
            endpoint(vec![(200, "application/json", repeated_call_body())]).await;
        let response = gemini_client(origin)
            .chat(&search_tools_request())
            .await
            .unwrap();
        assert_distinct_portable_ids(&response.tool_calls, "gemini-test");
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
            let body = captured.recv().await.unwrap().body;
            assert_gemini_accepts_function_turns(&body);
            assert_responses_name_their_calls(&body);
        }
    }

    /// Gemini's 400 for a malformed request reaches baybo as a bare body
    /// (unary) or rig's status display (stream); both must come out
    /// `BadRequest`, so the agent loop surfaces them instead of retrying.
    #[tokio::test]
    async fn an_invalid_argument_400_is_not_retried() {
        let rejection = json!({"error":{"code":400,"message":"function response parts mismatch","status":"INVALID_ARGUMENT"}}).to_string();
        let (origin, _captured) = endpoint(vec![
            (400, "application/json", rejection.clone()),
            (400, "application/json", rejection),
        ])
        .await;
        let client = gemini_client(origin);
        let request = mixed_history_request();
        let unary = client.chat(&request).await.unwrap_err();
        assert!(matches!(unary, crate::LlmError::BadRequest(_)), "{unary:?}");
        let streamed = match client.chat_stream(&request).await {
            Err(e) => e,
            Ok(mut stream) => stream.next().await.unwrap().unwrap_err(),
        };
        assert!(
            matches!(streamed, crate::LlmError::BadRequest(_)),
            "{streamed:?}"
        );
        assert!(!unary.is_retriable() && !streamed.is_retriable());
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
        let prepared = prepare_request(request, "gemini-test");
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

    fn stamped(model: &str, raw: &str) -> Option<String> {
        let mut signature = Some(raw.to_string());
        stamp_signature(&mut signature, model);
        signature
    }

    fn signed_use(id: &str, signature: Option<String>) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "web_search".into(),
            input: json!({}),
            signature,
        }
    }

    fn thinking(text: &str, signature: Option<String>) -> ContentBlock {
        ContentBlock::Thinking {
            id: None,
            content: vec![baybo_model::ThinkingContent::Text {
                text: text.into(),
                signature,
            }],
        }
    }

    /// One history turn per provenance case, each followed by its results.
    fn provenance_history_request() -> ChatRequest {
        let mut request = search_tools_request();
        let turns: Vec<Vec<ContentBlock>> = vec![
            vec![signed_use("own1", stamped("gemini-test", "own-sig"))],
            vec![
                signed_use("for1", stamped("other-model", "foreign-sig")),
                signed_use("for2", None),
            ],
            vec![signed_use("leg1", Some("legacy-sig".into()))],
            vec![signed_use("unsg1", None), signed_use("unsg2", None)],
            vec![
                thinking("mine", stamped("gemini-test", "own-thought")),
                signed_use("thk1", stamped("gemini-test", "own-call")),
            ],
            vec![
                thinking("theirs", stamped("other-model", "foreign-thought")),
                signed_use("thk2", stamped("other-model", "foreign-call")),
            ],
        ];
        for turn in turns {
            let ids: Vec<String> = turn
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            request.messages.push(ChatMessage::assistant(turn));
            request.messages.extend(ids.iter().map(|id| result(id)));
        }
        request
    }

    /// Per model content: the thought texts kept and each functionCall's
    /// `thoughtSignature`.
    type TurnSignatures = (Vec<(String, Option<String>)>, Vec<Option<String>>);

    fn model_turn_signatures(body: &Value) -> Vec<TurnSignatures> {
        let sig = |part: &Value| part["thoughtSignature"].as_str().map(str::to_string);
        body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["role"] == "model")
            .map(|c| {
                let parts = c["parts"].as_array().unwrap();
                let thoughts = parts
                    .iter()
                    .filter(|p| p["thought"] == true)
                    .map(|p| (p["text"].as_str().unwrap_or_default().to_string(), sig(p)))
                    .collect();
                let calls = parts
                    .iter()
                    .filter(|p| p.get("functionCall").is_some())
                    .map(sig)
                    .collect();
                (thoughts, calls)
            })
            .collect()
    }

    /// A signature reaches Gemini only from the model that issued it. Any
    /// other one, stamped by another model or stored before provenance,
    /// becomes the placeholder; a turn left with no signed call gets the
    /// placeholder on its first call; foreign thinking is not sent.
    #[tokio::test]
    async fn signatures_replay_only_to_their_issuer() {
        let (origin, mut captured) = endpoint(vec![
            (200, "text/event-stream", repeated_call_sse()),
            (200, "application/json", repeated_call_body()),
        ])
        .await;
        let client = gemini_client(origin);
        let request = provenance_history_request();
        collect_calls(client.chat_stream(&request).await.unwrap()).await;
        client.chat(&request).await.unwrap();
        let placeholder = || Some(THOUGHT_SIGNATURE_PLACEHOLDER.to_string());
        let own = |s: &str| Some(s.to_string());
        let expected: Vec<TurnSignatures> = vec![
            (vec![], vec![own("own-sig")]),
            (vec![], vec![placeholder(), None]),
            (vec![], vec![placeholder()]),
            (vec![], vec![placeholder(), None]),
            (
                vec![("mine".into(), own("own-thought"))],
                vec![own("own-call")],
            ),
            (vec![], vec![placeholder()]),
        ];
        for _ in 0..2 {
            let body = captured.recv().await.unwrap().body;
            assert_gemini_accepts_function_turns(&body);
            assert_eq!(model_turn_signatures(&body), expected);
        }
    }

    #[test]
    fn a_stamp_names_its_issuer() {
        let signature = stamped("gemini-test", "raw");
        let signature = signature.as_deref().unwrap();
        assert_eq!(own_signature(signature, "gemini-test"), Some("raw"));
        assert_eq!(own_signature(signature, "gemini-test-2"), None);
        assert_eq!(own_signature("raw", "gemini-test"), None);
    }

    /// A turn whose only part is foreign thinking keeps the thought with
    /// its signature cleared rather than becoming an empty model turn.
    #[test]
    fn foreign_thinking_alone_is_kept_unsigned() {
        let mut content = OneOrMany::one(AssistantContent::Reasoning(
            Reasoning::new_with_signature("theirs", stamped("other-model", "sig")),
        ));
        drop_foreign_thinking(&mut content, "gemini-test");
        replay_signatures(&mut content, "gemini-test");
        let AssistantContent::Reasoning(reasoning) = content.first() else {
            panic!("thinking was dropped");
        };
        assert_eq!(reasoning.first_signature(), None);
    }
}
