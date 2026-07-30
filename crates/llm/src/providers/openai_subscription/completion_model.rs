//! Codex Responses API client driven by an OAuth bearer.
//! Endpoint: `<base_url>/codex/responses`. See
//! `docs/modules/llm-openai-subscription.md` for design rationale.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use futures::stream::{self, Stream, StreamExt};
use rig::OneOrMany;
use rig::completion::message::{
    AssistantContent, Document, DocumentMediaType, DocumentSourceKind, Image, ImageDetail,
    MimeType, Reasoning, ReasoningContent, Text, ToolCall, ToolFunction, UserContent,
};
use rig::completion::{self, CompletionError, CompletionRequest};
use rig::message::Message;
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::oauth::ORIGINATOR;
use super::refresh_coordinator::{BackgroundRefresh, RefreshCoordinator};
use super::token_bundle::OAuthTokenBundle;
use super::token_store::VaultTokenStore;
use crate::tool_name::{sanitize_tool_name, unsanitize_tool_name};
use crate::{DOCUMENT_FILENAME_PARAM, LlmError, LlmStream, StreamEvent, TokenUsage, ToolCallInfo};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const RESPONSES_PATH: &str = "/codex/responses";

#[derive(Clone)]
pub struct OpenAiSubscriptionCompletionModel {
    model: String,
    base_url: String,
    /// Reasoning effort the operator picked, clamped to whatever the
    /// model accepts. `None` = reasoning off. Resolved once at
    /// construction time so every request body shares the same value.
    reasoning_effort: Option<&'static str>,
    /// Request transport for `<base_url>/codex/*`. NOT the refresh client —
    /// refresh lives on the coordinator and targets the OAuth issuer.
    http: reqwest::Client,
    /// Shared per-credential refresh state. Every client built from the
    /// same vault credential — the entry's default model, each
    /// `model_list` model, every hot-reload generation, every admin
    /// probe — resolves to the SAME coordinator, which is what makes the
    /// single-flight gate process-wide as the spec requires.
    refresh: Arc<RefreshCoordinator>,
}

impl OpenAiSubscriptionCompletionModel {
    pub fn new(
        model: String,
        base_url: Option<String>,
        reasoning_effort: Option<&str>,
        token_store: VaultTokenStore,
        http: reqwest::Client,
        background: BackgroundRefresh,
    ) -> Self {
        let base_url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let reasoning_effort = super::reasoning::resolve_effort(&model, reasoning_effort);
        Self {
            model,
            base_url,
            reasoning_effort,
            refresh: RefreshCoordinator::shared(token_store, http.clone(), background),
            http,
        }
    }

    pub fn base_url_is_default(&self) -> bool {
        self.base_url == DEFAULT_BASE_URL
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// rig-shape completion: drives the stream internally and assembles a
    /// non-streaming response. Single network call regardless.
    pub async fn completion(
        &self,
        request: CompletionRequest,
        effort: Option<&str>,
    ) -> Result<completion::CompletionResponse<()>, CompletionError> {
        let stream = self.stream(request, effort).await?;
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut thinking_blocks: Vec<baybo_model::ContentBlock> = Vec::new();
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            match event.map_err(|e| CompletionError::ProviderError(e.to_string()))? {
                StreamEvent::Text(chunk) => text_buf.push_str(&chunk),
                StreamEvent::Reasoning(delta) => reasoning_buf.push_str(&delta),
                StreamEvent::ThinkingBlock(block) => thinking_blocks.push(block),
                StreamEvent::ToolCall(call) => tool_calls.push(call),
                StreamEvent::Usage(u) => usage = u,
            }
        }

        // Choice order: reasoning first, then text, then tool calls — matches
        // the agent loop's expectation when it reads `AssistantContent` and
        // converts to `LlmResponse.thinking` + content.
        let mut choice_parts: Vec<AssistantContent> = Vec::new();
        if let Some(reasoning) = build_reasoning_block(reasoning_buf, thinking_blocks) {
            choice_parts.push(AssistantContent::Reasoning(reasoning));
        }
        if !text_buf.is_empty() {
            choice_parts.push(AssistantContent::Text(Text { text: text_buf }));
        }
        for tc in tool_calls {
            choice_parts.push(AssistantContent::ToolCall(ToolCall {
                id: tc.id,
                call_id: None,
                function: ToolFunction {
                    name: tc.name,
                    arguments: tc.arguments,
                },
                signature: tc.signature,
                additional_params: None,
            }));
        }
        if choice_parts.is_empty() {
            choice_parts.push(AssistantContent::Text(Text {
                text: String::new(),
            }));
        }
        let first = choice_parts.remove(0);
        let mut choice = OneOrMany::one(first);
        for part in choice_parts {
            choice.push(part);
        }

        Ok(completion::CompletionResponse {
            choice,
            usage: completion::Usage {
                input_tokens: usage.input_tokens as u64,
                output_tokens: usage.output_tokens as u64,
                total_tokens: (usage.input_tokens + usage.output_tokens) as u64,
                cached_input_tokens: usage.cached_input_tokens as u64,
                cache_creation_input_tokens: usage.cache_creation_input_tokens as u64,
                reasoning_tokens: 0,
            },
            raw_response: (),
            message_id: None,
        })
    }

    /// Open a streaming connection to `<base_url>/codex/responses` with
    /// pre-flight + reactive (401-once) refresh handling.
    pub async fn stream(
        &self,
        request: CompletionRequest,
        effort: Option<&str>,
    ) -> Result<LlmStream, CompletionError> {
        // A per-request effort (the session's thinking-level pin) overrides
        // the client's construction-time entry default, clamped to what this
        // model allows; `None` keeps the entry default. This is what makes the
        // chat header's thinking picker per-SESSION rather than a global edit.
        let effective_effort = match effort {
            Some(requested) => super::reasoning::resolve_effort(&self.model, Some(requested)),
            None => self.reasoning_effort,
        };
        let body =
            build_responses_body(&self.model, effective_effort, &request).map_err(|msg| {
                let err: Box<dyn std::error::Error + Send + Sync> = msg.into();
                CompletionError::RequestError(err)
            })?;
        let bundle = self
            .refresh
            .ensure_fresh_bundle()
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;
        let response = self
            .send(&bundle, &body)
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;
        let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            warn!(
                event = "openai_subscription_unauthorized",
                "Codex returned 401 — refreshing token and retrying once"
            );
            let refreshed = self
                .refresh
                .force_refresh(&bundle)
                .await
                .map_err(|e| CompletionError::ProviderError(e.to_string()))?;
            let retried = self
                .send(&refreshed, &body)
                .await
                .map_err(|e| CompletionError::ProviderError(e.to_string()))?;
            if retried.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(CompletionError::ProviderError(
                    "openai-subscription: unauthorized after refresh — run `baybo llm edit` \
                     and pick `OAuth login (re-authenticate)` for this entry"
                        .into(),
                ));
            }
            retried
        } else {
            response
        };
        Ok(self.adapt_response(response))
    }

    /// Live model discovery against `<base>/codex/models`.
    pub async fn list_remote_models(&self) -> crate::Result<Vec<crate::LiveModelInfo>> {
        let url = format!(
            "{}/codex/models?client_version={}",
            self.base_url,
            env!("CARGO_PKG_VERSION")
        );
        let bundle = self.refresh.ensure_fresh_bundle().await?;
        let resp = self.send_models_get(&url, &bundle).await?;
        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let refreshed = self.refresh.force_refresh(&bundle).await?;
            self.send_models_get(&url, &refreshed).await?
        } else {
            resp
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::status_to_error(
                status.as_u16(),
                format!("openai-subscription: GET /codex/models returned {status}: {body}"),
            ));
        }
        parse_models_response(resp).await
    }

    async fn send_models_get(
        &self,
        url: &str,
        bundle: &OAuthTokenBundle,
    ) -> crate::Result<reqwest::Response> {
        self.authed_request(reqwest::Method::GET, url, bundle)
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "openai-subscription: GET /codex/models"))
    }

    /// Apply the bearer + originator + (optional) account-id headers
    /// shared by every authenticated request. Returns a `RequestBuilder`
    /// the caller chains `.json()` / `.send()` onto.
    fn authed_request(
        &self,
        method: reqwest::Method,
        url: &str,
        bundle: &OAuthTokenBundle,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(&bundle.access_token)
            .header("originator", ORIGINATOR);
        if let Some(account_id) = &bundle.account_id {
            req = req.header("ChatGPT-Account-Id", account_id);
        }
        req
    }

    async fn send(
        &self,
        bundle: &OAuthTokenBundle,
        body: &Value,
    ) -> crate::Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, RESPONSES_PATH);
        debug!(url = %url, "POST openai-subscription Responses");
        self.authed_request(reqwest::Method::POST, &url, bundle)
            .header("OpenAI-Beta", "responses=experimental")
            .header("Accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "openai-subscription: HTTP transport"))
    }

    fn adapt_response(&self, response: reqwest::Response) -> LlmStream {
        let status = response.status();
        if !status.is_success() {
            // Surface as a single-element error stream so callers see the
            // provider message rather than an empty stream.
            let stream = stream::once(async move {
                let body = response.text().await.unwrap_or_default();
                Err(crate::status_to_error(
                    status.as_u16(),
                    format!("openai-subscription: Codex Responses returned {status}: {body}"),
                ))
            });
            return LlmStream::from_inner(Box::pin(stream));
        }
        LlmStream::from_inner(Box::pin(parse_sse_stream(response)))
    }
}

/// rig `CompletionRequest` → Codex Responses API JSON body. Returns
/// `Value` so tests can inspect the shape without mocking HTTP.
///
/// `reasoning_effort` is the operator's already-resolved effort
/// level (clamped to whatever the model supports). When `Some`, the
/// body includes `reasoning: { effort, summary: "auto" }` and
/// `include: ["reasoning.encrypted_content"]` so the server emits +
/// retains thinking state. `None` disables reasoning entirely.
pub(crate) fn build_responses_body(
    model: &str,
    reasoning_effort: Option<&str>,
    request: &CompletionRequest,
) -> Result<Value, String> {
    let mut input: Vec<Value> = Vec::new();
    for message in request.chat_history.iter() {
        for item in convert_message(message)? {
            input.push(item);
        }
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": sanitize_tool_name(&t.name),
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();

    // The Codex Responses API rejects requests without `instructions`
    // ("400: Instructions are required") even when the caller hasn't
    // set a system message — `baybo llm probe` is the canonical example.
    // Always supply at least a minimal placeholder.
    let instructions = request
        .preamble
        .clone()
        .unwrap_or_else(|| "You are a helpful assistant.".to_string());
    // store=false: we manage conversation state ourselves; don't ask
    // the server to retain it (matches the Codex CLI posture).
    let mut body = json!({
        "model": model,
        "input": input,
        "instructions": instructions,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "stream": true,
        "store": false,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(effort) = reasoning_effort {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
        // Tell the server to emit and retain encrypted reasoning
        // state across turns. Without `include`, multi-turn calls
        // can't echo prior thinking back, defeating the cache.
        body["include"] = json!(["reasoning.encrypted_content"]);
        debug!(
            model = %model,
            effort = %effort,
            "openai-subscription: reasoning enabled in request body"
        );
    } else {
        debug!(
            model = %model,
            "openai-subscription: reasoning disabled (effort = None)"
        );
    }
    // `temperature` is not part of the Codex Responses parameter set —
    // server returns 400 "Unsupported parameter: temperature" if we
    // forward it. Reasoning models pin sampling internally; the
    // request shape simply doesn't expose a knob.
    Ok(body)
}

async fn parse_models_response(
    resp: reqwest::Response,
) -> crate::Result<Vec<crate::LiveModelInfo>> {
    let body: Value = resp.json().await.map_err(|e| {
        LlmError::Decode(format!(
            "openai-subscription: /codex/models response parse: {e}"
        ))
    })?;
    project_models_body(body)
}

/// Project Codex's `{ "models": [...] }` into `Vec<LiveModelInfo>`. Each
/// raw entry is stashed verbatim into `extras` so operators with --json
/// can see fields baybo doesn't surface.
fn project_models_body(mut body: Value) -> crate::Result<Vec<crate::LiveModelInfo>> {
    let raw = match body.get_mut("models").map(Value::take) {
        Some(Value::Array(arr)) => arr,
        _ => {
            return Err(LlmError::Decode(format!(
                "openai-subscription: /codex/models missing `models` array; body: {body}"
            )));
        }
    };
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let id = entry
            .get("slug")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                LlmError::Decode(format!(
                    "openai-subscription: model entry missing `slug`: {entry}"
                ))
            })?;
        let display_name = entry
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty());
        let context_window = entry
            .get("context_window")
            .and_then(Value::as_u64)
            .or_else(|| entry.get("max_context_window").and_then(Value::as_u64))
            .map(|v| v as usize);
        let supports_vision = entry
            .get("input_modalities")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .any(|m| m.as_str() == Some("image") || m.as_str() == Some("Image"))
            });
        // All Codex-served models are tool-capable; `supports_parallel_tool_calls`
        // is a different question and shouldn't be conflated.
        let supports_tools = Some(true);
        out.push(crate::LiveModelInfo {
            id,
            display_name,
            description,
            context_window,
            supports_vision,
            supports_tools,
            extras: entry,
        });
    }
    Ok(out)
}

fn convert_message(message: &Message) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    match message {
        Message::User { content } => {
            // Split into one Responses API item per tool result (function_call_output)
            // plus one combined message item for text/image parts.
            let mut text_parts: Vec<Value> = Vec::new();
            for item in content.iter() {
                match item {
                    UserContent::Text(t) => {
                        text_parts.push(json!({
                            "type": "input_text",
                            "text": t.text,
                        }));
                    }
                    UserContent::ToolResult(tr) => {
                        let text_output = tool_result_to_text(tr);
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": tr.id,
                            "output": text_output,
                        }));
                    }
                    UserContent::Image(image) => {
                        text_parts.push(convert_image(image)?);
                    }
                    UserContent::Document(document) => {
                        text_parts.push(convert_document(document)?);
                    }
                    UserContent::Audio(_) | UserContent::Video(_) => {
                        text_parts.push(json!({
                            "type": "input_text",
                            "text": "[unsupported non-text user content elided]",
                        }));
                    }
                }
            }
            if !text_parts.is_empty() {
                out.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": text_parts,
                }));
            }
        }
        Message::Assistant { content, .. } => {
            let mut text_parts: Vec<Value> = Vec::new();
            for item in content.iter() {
                match item {
                    AssistantContent::Text(t) => {
                        text_parts.push(json!({
                            "type": "output_text",
                            "text": t.text,
                        }));
                    }
                    AssistantContent::ToolCall(call) => {
                        out.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": sanitize_tool_name(&call.function.name),
                            "arguments": call.function.arguments.to_string(),
                        }));
                    }
                    AssistantContent::Reasoning(reasoning) => {
                        // Echo the redacted reasoning item we captured
                        // last turn straight back into `input` so the
                        // server can decode `encrypted_content` and
                        // resume the thinking state. The stored bytes
                        // are an opaque server-side blob; we don't
                        // synthesise a new shape, just round-trip the
                        // exact item we received.
                        for content in reasoning.content.iter() {
                            if let rig::completion::message::ReasoningContent::Redacted { data } =
                                content
                                && let Ok(item) = serde_json::from_str::<Value>(data)
                            {
                                out.push(item);
                            }
                        }
                    }
                    AssistantContent::Image(_) => {
                        text_parts.push(json!({
                            "type": "output_text",
                            "text": "[assistant-generated image elided]",
                        }));
                    }
                }
            }
            if !text_parts.is_empty() {
                out.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": text_parts,
                }));
            }
        }
        // System messages normally flow through `preamble` -> `instructions`.
        // Codex Responses API has no system-role item; emit as user text so
        // a stray System variant isn't silently dropped.
        Message::System { content } => {
            out.push(json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": content }],
            }));
        }
    }
    Ok(out)
}

fn convert_image(image: &Image) -> Result<Value, String> {
    let detail = match image.detail.as_ref().unwrap_or(&ImageDetail::Auto) {
        ImageDetail::Low => "low",
        ImageDetail::High => "high",
        ImageDetail::Auto => "auto",
    };
    match &image.data {
        DocumentSourceKind::Url(image_url) => Ok(json!({
            "type": "input_image",
            "image_url": image_url,
            "detail": detail,
        })),
        DocumentSourceKind::Base64(data) => {
            let mime_type = image.media_type.as_ref().ok_or_else(|| {
                "openai-subscription: base64 image is missing its media type".to_string()
            })?;
            Ok(json!({
                "type": "input_image",
                "image_url": format!(
                    "data:{};base64,{data}",
                    mime_type.to_mime_type()
                ),
                "detail": detail,
            }))
        }
        DocumentSourceKind::FileId(file_id) => Ok(json!({
            "type": "input_image",
            "file_id": file_id,
            "detail": detail,
        })),
        _ => Err(
            "openai-subscription: image source must be a URL, base64 payload, or file id"
                .to_string(),
        ),
    }
}

fn convert_document(document: &Document) -> Result<Value, String> {
    if document.media_type != Some(DocumentMediaType::PDF) {
        return Err("openai-subscription: only PDF documents are supported".to_string());
    }
    match &document.data {
        DocumentSourceKind::Base64(data) => {
            let filename = document
                .additional_params
                .as_ref()
                .and_then(|params| params.get(DOCUMENT_FILENAME_PARAM))
                .and_then(Value::as_str)
                .filter(|filename| !filename.is_empty())
                .unwrap_or("document.pdf");
            Ok(json!({
                "type": "input_file",
                "file_data": format!("data:application/pdf;base64,{data}"),
                "filename": filename,
            }))
        }
        DocumentSourceKind::Url(file_url) => Ok(json!({
            "type": "input_file",
            "file_url": file_url,
        })),
        _ => Err("openai-subscription: PDF source must be a URL or base64 payload".to_string()),
    }
}

/// Combine streamed reasoning deltas + any complete `Thinking` blocks
/// into one `Reasoning` block for `AssistantContent`. Returns `None`
/// when there's nothing to surface.
///
/// `reasoning_summary_text.delta` streams the *same* prose that the
/// finalised `output_item.done` reasoning item then repeats under
/// `summary[]`, so keeping both yields the summary twice — once from
/// each source. The finalised item wins (it is complete even when the
/// deltas were cut short); the delta buffer only fills in when no item
/// carried a summary at all.
fn build_reasoning_block(
    reasoning_buf: String,
    thinking_blocks: Vec<baybo_model::ContentBlock>,
) -> Option<Reasoning> {
    let mut content: Vec<ReasoningContent> = Vec::new();
    let mut saw_item_summary = false;
    for block in thinking_blocks {
        if let baybo_model::ContentBlock::Thinking { content: tc, .. } = block {
            for piece in tc {
                content.push(match piece {
                    baybo_model::ThinkingContent::Text { text, signature } => {
                        ReasoningContent::Text { text, signature }
                    }
                    baybo_model::ThinkingContent::Summary { text } => {
                        saw_item_summary = true;
                        ReasoningContent::Summary(text)
                    }
                    baybo_model::ThinkingContent::Redacted { data } => {
                        ReasoningContent::Redacted { data }
                    }
                });
            }
        }
    }
    if !reasoning_buf.is_empty() && !saw_item_summary {
        // Codex reasoning deltas are summary-style (no signature payload).
        content.insert(0, ReasoningContent::Summary(reasoning_buf));
    }
    if content.is_empty() {
        return None;
    }
    let mut reasoning = Reasoning::new("");
    reasoning.content = content;
    Some(reasoning)
}

fn tool_result_to_text(tr: &completion::message::ToolResult) -> String {
    use completion::message::ToolResultContent;
    let mut buf = String::new();
    for piece in tr.content.iter() {
        match piece {
            ToolResultContent::Text(t) => buf.push_str(&t.text),
            _ => buf.push_str("[non-text tool result content elided]"),
        }
    }
    buf
}

/// SSE → flat `Stream<Result<StreamEvent>>`. Drops unknown event types;
/// Codex frequently adds new ones and we'd rather skip them than fail.
fn parse_sse_stream(
    response: reqwest::Response,
) -> impl Stream<Item = crate::Result<StreamEvent>> + Send {
    use bytes::BytesMut;
    let mut buffer = BytesMut::new();
    let mut function_calls: HashMap<String, FunctionCallAccumulator> = HashMap::new();
    let mut byte_stream = response.bytes_stream();

    async_stream::stream! {
        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    yield Err(LlmError::Transient(format!(
                        "openai-subscription: SSE transport: {e}"
                    )));
                    return;
                }
            };
            buffer.extend_from_slice(&chunk);
            // SSE events are separated by blank lines (\n\n). Pull all
            // complete events out of the buffer.
            while let Some(idx) = find_event_boundary(&buffer) {
                let raw = buffer.split_to(idx + 2);
                let raw_str = match std::str::from_utf8(&raw) {
                    Ok(s) => s,
                    Err(e) => {
                        yield Err(LlmError::Decode(format!(
                            "openai-subscription: SSE non-UTF-8: {e}"
                        )));
                        return;
                    }
                };
                let Some(event) = parse_sse_event(raw_str) else { continue };
                match translate_event(&mut function_calls, event) {
                    Some(events) => {
                        for ev in events {
                            yield ev;
                        }
                    }
                    None => continue,
                }
            }
        }
    }
}

fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

#[derive(Debug, PartialEq, Eq)]
struct SseEvent {
    event_type: String,
    data: String,
}

fn parse_sse_event(raw: &str) -> Option<SseEvent> {
    let mut event_type = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        } else if line.starts_with(':') {
            // Comment / heartbeat — ignore.
            continue;
        }
    }
    if event_type.is_empty() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event_type,
        data: data_lines.join("\n"),
    })
}

#[derive(Default)]
struct FunctionCallAccumulator {
    name: String,
    arguments: String,
}

fn translate_event(
    function_calls: &mut HashMap<String, FunctionCallAccumulator>,
    event: SseEvent,
) -> Option<Vec<crate::Result<StreamEvent>>> {
    let payload: Value = serde_json::from_str(&event.data).ok()?;
    let mut out: Vec<crate::Result<StreamEvent>> = Vec::new();
    match event.event_type.as_str() {
        // Plain text chunks coming back from the model.
        "response.output_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                out.push(Ok(StreamEvent::Text(delta.to_owned())));
            }
        }
        // Reasoning summary deltas (gpt-5 family).
        "response.reasoning.delta"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                debug!(
                    event_type = %event.event_type,
                    bytes = delta.len(),
                    "openai-subscription: reasoning delta"
                );
                out.push(Ok(StreamEvent::Reasoning(delta.to_owned())));
            }
        }
        // Tool / function call assembly. Codex emits an `added` event with
        // the call's name + id, then a stream of `arguments.delta`s, then a
        // `done`/`completed` event.
        "response.output_item.added" => {
            if let Some(item) = payload.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned();
                if !call_id.is_empty() {
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    function_calls
                        .entry(call_id)
                        .or_insert_with(|| FunctionCallAccumulator {
                            name,
                            arguments: String::new(),
                        });
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let call_id = payload
                .get("call_id")
                .or_else(|| payload.get("item_id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let delta = payload.get("delta").and_then(Value::as_str).unwrap_or("");
            if !call_id.is_empty() {
                let entry = function_calls.entry(call_id.to_owned()).or_default();
                entry.arguments.push_str(delta);
            }
        }
        "response.function_call_arguments.done" => {
            emit_function_call(&payload, function_calls, &mut out);
        }
        "response.output_item.done" => {
            // Dispatch on the carried item type — `output_item.done`
            // is a generic "this item is finalised" marker; the
            // item's `type` field tells us what shape it is.
            let item_type = payload
                .get("item")
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match item_type {
                "function_call" => emit_function_call(&payload, function_calls, &mut out),
                "reasoning" => {
                    debug!("openai-subscription: reasoning output_item.done received");
                    // The whole `item` is what we need to echo back next
                    // turn so the server can decode `encrypted_content`
                    // and resume thinking — store it in the redacted
                    // signature slot. Also surface the human-readable
                    // summary as plain `Reasoning` text for any UI that
                    // wants to display it without round-trip semantics.
                    if let Some(item) = payload.get("item") {
                        let id = item.get("id").and_then(Value::as_str).map(str::to_owned);
                        let summary_text = item
                            .get("summary")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        // Whole item as the redacted payload — the
                        // server treats it as opaque on the next turn,
                        // so we just round-trip the bytes.
                        let data = serde_json::to_string(item).unwrap_or_default();
                        let mut content = Vec::<baybo_model::ThinkingContent>::with_capacity(2);
                        if !summary_text.is_empty() {
                            content
                                .push(baybo_model::ThinkingContent::Summary { text: summary_text });
                        }
                        content.push(baybo_model::ThinkingContent::Redacted { data });
                        out.push(Ok(StreamEvent::ThinkingBlock(
                            baybo_model::ContentBlock::Thinking { id, content },
                        )));
                    }
                }
                _ => {}
            }
        }
        // Final event with usage totals.
        "response.completed" => {
            if let Some(usage) = payload.get("response").and_then(|r| r.get("usage")) {
                let input = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let output = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                // OpenAI Responses API reports prompt-cache hits under
                // `input_tokens_details.cached_tokens`. There is no
                // cache-write counter — the API doesn't separate cache
                // creation from regular input.
                let cached = usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                out.push(Ok(StreamEvent::Usage(TokenUsage {
                    input_tokens: input,
                    output_tokens: output,
                    cached_input_tokens: cached,
                    cache_creation_input_tokens: 0,
                })));
            }
        }
        "response.error" | "error" => {
            let message = payload
                .get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| payload.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("openai-subscription: server-side stream error");
            // Mid-stream `response.error` / `error` events from Codex
            // are typically transport / overload signals (the request
            // was accepted at the HTTP layer but the upstream model
            // run failed). Mark Transient so the caller's retry path
            // can re-attempt; non-transient errors usually surface as
            // non-success HTTP up in `adapt_response`.
            out.push(Err(LlmError::Transient(message.to_owned())));
        }
        other => {
            // Unknown event type — drop the payload but log the
            // type at debug so operators can spot when Codex
            // introduces a shape we don't parse yet (the reasoning
            // family in particular has gone through several
            // rename rounds).
            debug!(
                event_type = %other,
                "openai-subscription: unhandled stream event (dropping)"
            );
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Assemble a `function_call` accumulator into a `ToolCall` event.
/// Pulled out of `translate_event` so both `output_item.done` and
/// `function_call_arguments.done` can drive it.
fn emit_function_call(
    payload: &Value,
    function_calls: &mut HashMap<String, FunctionCallAccumulator>,
    out: &mut Vec<crate::Result<StreamEvent>>,
) {
    let Some(call_id) = payload
        .get("call_id")
        .or_else(|| payload.get("item").and_then(|i| i.get("call_id")))
        .or_else(|| payload.get("item").and_then(|i| i.get("id")))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let acc = function_calls.remove(&call_id).unwrap_or_default();
    let arguments_str = if acc.arguments.is_empty() {
        payload
            .get("item")
            .and_then(|i| i.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    } else {
        acc.arguments
    };
    let name = if acc.name.is_empty() {
        payload
            .get("item")
            .and_then(|i| i.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    } else {
        acc.name
    };
    if name.is_empty() {
        return;
    }
    let arguments_value: Value =
        serde_json::from_str(&arguments_str).unwrap_or(Value::String(arguments_str));
    out.push(Ok(StreamEvent::ToolCall(ToolCallInfo {
        id: call_id,
        name: unsanitize_tool_name(&name),
        arguments: arguments_value,
        signature: None,
    })));
}

impl LlmStream {
    pub(crate) fn from_inner(
        inner: Pin<Box<dyn Stream<Item = crate::Result<StreamEvent>> + Send>>,
    ) -> Self {
        Self { inner }
    }
}

// Compile-time bound check: shared via Arc, so must stay Send+Sync+Clone.
#[allow(dead_code)]
fn _assert_bounds() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<OpenAiSubscriptionCompletionModel>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::ToolDefinition;
    use serde_json::json;

    fn empty_request() -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(Message::User {
                content: OneOrMany::one(UserContent::Text(Text { text: "hi".into() })),
            }),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        }
    }

    #[test]
    fn body_carries_model_and_text_user_input() {
        let body = build_responses_body("gpt-5", None, &empty_request()).unwrap();
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn body_preserves_base64_image_as_input_image() {
        let req = CompletionRequest {
            chat_history: OneOrMany::one(Message::User {
                content: OneOrMany::many(vec![
                    UserContent::Text(Text {
                        text: "what is this?".into(),
                    }),
                    UserContent::Image(Image {
                        data: DocumentSourceKind::Base64("AQID".into()),
                        media_type: Some(rig::completion::message::ImageMediaType::PNG),
                        detail: Some(ImageDetail::Auto),
                        additional_params: None,
                    }),
                ])
                .unwrap(),
            }),
            ..empty_request()
        };
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,AQID");
        assert_eq!(content[1]["detail"], "auto");
    }

    #[test]
    fn body_preserves_image_url_and_detail() {
        let req = CompletionRequest {
            chat_history: OneOrMany::one(Message::User {
                content: OneOrMany::one(UserContent::Image(Image {
                    data: DocumentSourceKind::Url("https://example.test/image.webp".into()),
                    media_type: None,
                    detail: Some(ImageDetail::High),
                    additional_params: None,
                })),
            }),
            ..empty_request()
        };
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let image = &body["input"][0]["content"][0];
        assert_eq!(image["type"], "input_image");
        assert_eq!(image["image_url"], "https://example.test/image.webp");
        assert_eq!(image["detail"], "high");
    }

    #[test]
    fn body_rejects_base64_image_without_media_type() {
        let req = CompletionRequest {
            chat_history: OneOrMany::one(Message::User {
                content: OneOrMany::one(UserContent::Image(Image {
                    data: DocumentSourceKind::Base64("AQID".into()),
                    media_type: None,
                    detail: None,
                    additional_params: None,
                })),
            }),
            ..empty_request()
        };
        let err = build_responses_body("gpt-5", None, &req).unwrap_err();
        assert!(err.contains("missing its media type"), "{err}");
    }

    #[test]
    fn body_preserves_base64_pdf_as_named_input_file() {
        let req = CompletionRequest {
            chat_history: OneOrMany::one(Message::User {
                content: OneOrMany::one(UserContent::Document(Document {
                    data: DocumentSourceKind::Base64("AQID".into()),
                    media_type: Some(DocumentMediaType::PDF),
                    additional_params: Some(json!({"filename": "quarterly-report.pdf"})),
                })),
            }),
            ..empty_request()
        };
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let document = &body["input"][0]["content"][0];
        assert_eq!(document["type"], "input_file");
        assert_eq!(document["file_data"], "data:application/pdf;base64,AQID");
        assert_eq!(document["filename"], "quarterly-report.pdf");
    }

    #[test]
    fn body_preserves_pdf_url_as_input_file() {
        let req = CompletionRequest {
            chat_history: OneOrMany::one(Message::User {
                content: OneOrMany::one(UserContent::Document(Document {
                    data: DocumentSourceKind::Url("https://example.test/report.pdf".into()),
                    media_type: Some(DocumentMediaType::PDF),
                    additional_params: None,
                })),
            }),
            ..empty_request()
        };
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let document = &body["input"][0]["content"][0];
        assert_eq!(document["type"], "input_file");
        assert_eq!(document["file_url"], "https://example.test/report.pdf");
        assert!(document.get("file_data").is_none());
    }

    #[test]
    fn body_lifts_preamble_into_instructions() {
        let mut req = empty_request();
        req.preamble = Some("be terse".into());
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        assert_eq!(body["instructions"], "be terse");
    }

    /// Regression: the Codex Responses API rejects requests without
    /// `instructions` (400 "Instructions are required"). `baybo llm
    /// probe` builds a request with no preamble, so the body builder
    /// must inject a placeholder when the caller didn't provide one.
    #[test]
    fn body_supplies_default_instructions_when_preamble_is_absent() {
        let req = empty_request();
        assert!(req.preamble.is_none());
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let instructions = body["instructions"]
            .as_str()
            .expect("instructions must be present");
        assert!(!instructions.is_empty());
    }

    /// Regression: Codex Responses returns 400 "Unsupported parameter:
    /// temperature" when the field is forwarded. The reasoning models
    /// pin sampling server-side; we drop the field even when the
    /// caller (e.g. `baybo llm probe`) sets one.
    #[test]
    fn body_drops_temperature_for_codex_responses() {
        let mut req = empty_request();
        req.temperature = Some(0.0);
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        assert!(
            body.get("temperature").is_none(),
            "temperature must not be forwarded; got body = {body}"
        );
    }

    #[test]
    fn body_emits_reasoning_when_effort_is_set() {
        let req = empty_request();
        let body = build_responses_body("gpt-5", Some("high"), &req).unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn body_omits_reasoning_when_effort_is_none() {
        let req = empty_request();
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        assert!(
            body.get("reasoning").is_none(),
            "reasoning must be absent when effort is None"
        );
        assert!(
            body.get("include").is_none(),
            "include must be absent when reasoning is off"
        );
    }

    /// Regression: Codex Responses requires `tools[].name` to match
    /// `^[a-zA-Z0-9_-]+$`. Baybo's MCP-prefixed names (`browser/foo`)
    /// must be encoded on the way out and decoded on the way back.
    #[test]
    fn tool_name_round_trips_through_sanitization() {
        let original = "browser/navigate_page";
        let sanitized = sanitize_tool_name(original);
        assert!(
            sanitized
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "sanitized name must satisfy Codex regex: {sanitized}",
        );
        assert_eq!(unsanitize_tool_name(&sanitized), original);
    }

    #[test]
    fn body_sanitizes_namespaced_tool_names() {
        let mut req = empty_request();
        req.tools.push(ToolDefinition {
            name: "browser/take_screenshot".into(),
            description: "snap".into(),
            parameters: json!({"type":"object","properties":{}}),
        });
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "browser__take_screenshot");
    }

    #[test]
    fn body_translates_tools_to_function_shape() {
        let mut req = empty_request();
        req.tools.push(ToolDefinition {
            name: "search".into(),
            description: "look stuff up".into(),
            parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        });
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "search");
        assert_eq!(tools[0]["parameters"]["properties"]["q"]["type"], "string");
    }

    #[test]
    fn body_translates_assistant_tool_call_history() {
        let mut req = empty_request();
        req.chat_history.push(Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(ToolCall {
                id: "call_1".into(),
                call_id: None,
                function: ToolFunction {
                    name: "search".into(),
                    arguments: json!({"q": "rust"}),
                },
                signature: None,
                additional_params: None,
            })),
        });
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let input = body["input"].as_array().unwrap();
        let call_item = input
            .iter()
            .find(|i| i["type"] == "function_call")
            .expect("expected a function_call item");
        assert_eq!(call_item["call_id"], "call_1");
        assert_eq!(call_item["name"], "search");
        // arguments is a JSON-encoded string per Responses API contract.
        assert_eq!(call_item["arguments"], "{\"q\":\"rust\"}");
    }

    #[test]
    fn body_translates_tool_result_user_message() {
        let mut req = empty_request();
        req.chat_history.push(Message::User {
            content: OneOrMany::one(UserContent::ToolResult(completion::message::ToolResult {
                id: "call_1".into(),
                call_id: None,
                content: OneOrMany::one(completion::message::ToolResultContent::Text(Text {
                    text: "rust is a programming language".into(),
                })),
            })),
        });
        let body = build_responses_body("gpt-5", None, &req).unwrap();
        let input = body["input"].as_array().unwrap();
        let result_item = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .expect("expected a function_call_output item");
        assert_eq!(result_item["call_id"], "call_1");
        assert_eq!(result_item["output"], "rust is a programming language");
    }

    #[test]
    fn parse_sse_event_handles_event_and_data() {
        let raw = "event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n";
        let event = parse_sse_event(raw).unwrap();
        assert_eq!(event.event_type, "response.output_text.delta");
        assert_eq!(event.data, "{\"delta\":\"hi\"}");
    }

    #[test]
    fn parse_sse_event_ignores_comment_lines() {
        let raw = ":heartbeat\nevent: response.output_text.delta\ndata: {\"delta\":\"x\"}\n\n";
        let event = parse_sse_event(raw).unwrap();
        assert_eq!(event.event_type, "response.output_text.delta");
    }

    #[test]
    fn translate_event_emits_text_delta() {
        let mut calls = HashMap::new();
        let event = SseEvent {
            event_type: "response.output_text.delta".into(),
            data: r#"{"delta":"hello"}"#.into(),
        };
        let out = translate_event(&mut calls, event).unwrap();
        assert_eq!(out.len(), 1);
        match out.into_iter().next().unwrap().unwrap() {
            StreamEvent::Text(s) => assert_eq!(s, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn translate_event_assembles_function_call_from_deltas() {
        let mut calls = HashMap::new();
        // 1: added — registers id + name
        translate_event(
            &mut calls,
            SseEvent {
                event_type: "response.output_item.added".into(),
                data: r#"{"item":{"type":"function_call","call_id":"call_1","name":"search"}}"#
                    .into(),
            },
        );
        // 2 + 3: argument deltas
        translate_event(
            &mut calls,
            SseEvent {
                event_type: "response.function_call_arguments.delta".into(),
                data: r#"{"call_id":"call_1","delta":"{\"q\":"}"#.into(),
            },
        );
        translate_event(
            &mut calls,
            SseEvent {
                event_type: "response.function_call_arguments.delta".into(),
                data: r#"{"call_id":"call_1","delta":"\"rust\"}"}"#.into(),
            },
        );
        // 4: done — emits the assembled ToolCall
        let final_events = translate_event(
            &mut calls,
            SseEvent {
                event_type: "response.function_call_arguments.done".into(),
                data: r#"{"call_id":"call_1"}"#.into(),
            },
        )
        .unwrap();
        assert_eq!(final_events.len(), 1);
        match final_events.into_iter().next().unwrap().unwrap() {
            StreamEvent::ToolCall(tc) => {
                assert_eq!(tc.id, "call_1");
                assert_eq!(tc.name, "search");
                assert_eq!(tc.arguments, json!({"q": "rust"}));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn translate_event_emits_usage_from_completed() {
        let mut calls = HashMap::new();
        let out = translate_event(
            &mut calls,
            SseEvent {
                event_type: "response.completed".into(),
                data: r#"{"response":{"usage":{"input_tokens":42,"output_tokens":7}}}"#.into(),
            },
        )
        .unwrap();
        match out.into_iter().next().unwrap().unwrap() {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 42);
                assert_eq!(u.output_tokens, 7);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn translate_event_surfaces_error() {
        let mut calls = HashMap::new();
        let out = translate_event(
            &mut calls,
            SseEvent {
                event_type: "response.error".into(),
                data: r#"{"error":{"message":"rate limited"}}"#.into(),
            },
        )
        .unwrap();
        let err = out.into_iter().next().unwrap();
        assert!(matches!(err, Err(LlmError::Transient(ref m)) if m == "rate limited"));
    }

    #[test]
    fn build_reasoning_block_combines_deltas_and_thinking_blocks() {
        // No content -> None.
        assert!(build_reasoning_block(String::new(), Vec::new()).is_none());

        // Reasoning deltas alone -> single Summary entry.
        let r = build_reasoning_block("step 1\nstep 2".into(), Vec::new()).unwrap();
        assert_eq!(r.content.len(), 1);
        assert!(matches!(
            &r.content[0],
            ReasoningContent::Summary(s) if s == "step 1\nstep 2"
        ));

        // Reasoning deltas + a thinking block that carries NO summary ->
        // Summary + the block's pieces appended in order. Verifies neither
        // path drops content (the bug this regression test guards against).
        let block = baybo_model::ContentBlock::Thinking {
            id: Some("t1".into()),
            content: vec![
                baybo_model::ThinkingContent::Text {
                    text: "signed thought".into(),
                    signature: Some("sig".into()),
                },
                baybo_model::ThinkingContent::Redacted {
                    data: "secret".into(),
                },
            ],
        };
        let r = build_reasoning_block("delta".into(), vec![block]).unwrap();
        assert_eq!(r.content.len(), 3);
        assert!(matches!(&r.content[0], ReasoningContent::Summary(s) if s == "delta"));
        assert!(matches!(
            &r.content[1],
            ReasoningContent::Text { text, signature: Some(sig) }
                if text == "signed thought" && sig == "sig"
        ));
        assert!(matches!(&r.content[2], ReasoningContent::Redacted { data } if data == "secret"));
    }

    #[test]
    fn build_reasoning_block_drops_deltas_the_finalised_item_repeats() {
        // Codex streams `reasoning_summary_text.delta` and then repeats the
        // same prose in `output_item.done`'s `summary[]`. Keeping both
        // surfaced the summary twice ("**Title**\n**Title**") in the trace
        // and echoed it twice back to the provider.
        let summary = "**Planning live GitHub trending fetch**";
        let block = baybo_model::ContentBlock::Thinking {
            id: Some("rs_1".into()),
            content: vec![
                baybo_model::ThinkingContent::Summary {
                    text: summary.into(),
                },
                baybo_model::ThinkingContent::Redacted {
                    data: "encrypted".into(),
                },
            ],
        };
        let r = build_reasoning_block(summary.into(), vec![block]).unwrap();
        assert_eq!(r.content.len(), 2);
        assert!(matches!(&r.content[0], ReasoningContent::Summary(s) if s == summary));
        assert!(
            matches!(&r.content[1], ReasoningContent::Redacted { data } if data == "encrypted")
        );
    }

    #[test]
    fn project_models_body_lifts_codex_fields_into_live_model_info() {
        let body = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5",
                    "display_name": "GPT-5",
                    "description": "flagship",
                    "context_window": 272_000,
                    "supports_parallel_tool_calls": true,
                    "input_modalities": ["text", "image"],
                },
                {
                    "slug": "gpt-5-mini",
                    "display_name": "GPT-5 mini",
                    "description": "",
                    "max_context_window": 128_000,
                    "supports_parallel_tool_calls": false,
                    "supports_search_tool": true,
                    "input_modalities": ["text"],
                }
            ]
        });
        let entries = project_models_body(body).unwrap();
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].id, "gpt-5");
        assert_eq!(entries[0].display_name.as_deref(), Some("GPT-5"));
        assert_eq!(entries[0].description.as_deref(), Some("flagship"));
        assert_eq!(entries[0].context_window, Some(272_000));
        assert_eq!(entries[0].supports_vision, Some(true));
        assert_eq!(entries[0].supports_tools, Some(true));
        assert_eq!(
            entries[0].extras.get("slug").and_then(Value::as_str),
            Some("gpt-5")
        );

        assert_eq!(entries[1].id, "gpt-5-mini");
        assert!(entries[1].description.is_none());
        // max_context_window fallback when context_window is absent.
        assert_eq!(entries[1].context_window, Some(128_000));
        assert_eq!(entries[1].supports_vision, Some(false));
        // Tool-capability is constant for Codex-served models.
        assert_eq!(entries[1].supports_tools, Some(true));
    }

    #[test]
    fn project_models_body_errors_loudly_on_missing_models_array() {
        let body = serde_json::json!({"oops": "no models field"});
        let err = project_models_body(body).unwrap_err();
        match err {
            LlmError::Decode(msg) => assert!(msg.contains("missing `models` array")),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn project_models_body_errors_loudly_on_missing_slug() {
        let body = serde_json::json!({
            "models": [{ "display_name": "no slug here" }]
        });
        let err = project_models_body(body).unwrap_err();
        match err {
            LlmError::Decode(msg) => assert!(msg.contains("missing `slug`")),
            other => panic!("expected Decode, got {other:?}"),
        }
    }
}
