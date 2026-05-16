//! `WebFetch` — GET an `http(s)` URL, extract the article body, return text.
//!
//! Matches Claude Code's WebFetch tool shape: `{ url, prompt? }`. The
//! side-LLM extraction path fires when ALL three hold: a `prompt` is
//! supplied, [`ToolContext::llm`] is populated (the agent layer binds
//! a per-call [`BilledChat`] handle to the running
//! `(user, session, job, span)` so cost lands in `cost_records`
//! against the WebFetch tool span), AND the extracted text is at
//! least `SUMMARY_MIN_CHARS` long. Smaller pages fit in the agent's
//! own context, so the text is returned verbatim — a side-LLM call
//! would cost tokens for output that is almost always less faithful
//! than the original. Argv-mode boot and tests without an LLM binding
//! also fall through to the raw path.
//!
//! Raw output is capped at 256 KiB on a UTF-8 char boundary, matching the
//! per-tool cap documented in `docs/modules/security.md`. LLM input is capped
//! independently at 96 KiB so a giant page can't blow the side LLM's context.
//! The gateway applies a final cap on top of this; injection scanning and
//! `<tool_output>` wrapping are also the gateway's job — this tool just
//! returns the extracted (or summarised) text.
//!
//! HTML bodies are run through `dom_smoothie` — a Rust port of
//! Mozilla's Readability.js. It evaluates each candidate subtree by
//! text density and class/id signals, picks the article body, and
//! drops navigation chrome, sidebars, comments, scripts, and styles
//! in one pass. We ask for `TextMode::Formatted` so block boundaries
//! survive as newlines without injecting markdown syntax (the user
//! asked for plain text, not markdown). When the heuristic can't
//! identify an article (list pages, very short bodies, malformed
//! markup), the raw page text is returned as a fallback so the agent
//! still sees *something* — script tags are inert in plain text
//! output, so the security floor we lose is essentially nil.
//!
//! SSRF guard runs at two layers:
//!  1. URL parse: `validate_url_with` rejects non-http(s) schemes, literal-IP
//!     hosts in blocked ranges, and `localhost` aliases.
//!  2. DNS resolution: a custom `reqwest::dns::Resolve` (`SafeResolver`) runs
//!     `aura_security::is_blocked_ip` over every resolved address, so an
//!     attacker-controlled hostname that resolves to a private/loopback/
//!     metadata IP is dropped before the connector ever opens a socket.
//!     Each redirect hop triggers a fresh resolution, so DNS rebinding inside
//!     a single redirect chain is also caught.
//!
//! Redirect host pinning: redirects are restricted to the same host as
//! the original request. Cross-host redirects are surfaced as a
//! `ToolOutput::Error` so the LLM has to re-issue WebFetch on the new
//! URL — it makes the host change visible in the call trace instead of
//! letting it happen silently inside reqwest.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_llm::{BilledChat, ChatRequest};
use aura_model::{ChatMessage, ContentBlock, Role};
use dom_smoothie::{Config, Readability, TextMode};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use serde::Deserialize;
use serde_json::{Value, json};

use aura_trace::ToolEventPayload;

use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput, start_timer};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Cap on the rendered text passed into the side LLM when a `prompt` is
/// supplied. 96 KiB ≈ 24 K tokens — well inside every modern context
/// window after the system+prompt overhead, and capped low enough that a
/// pathological page can't dominate the per-call cost.
const MAX_SUMMARY_INPUT_BYTES: usize = 96 * 1024;
/// Threshold below which a `prompt` is ignored even when an LLM is
/// wired in. Short pages fit comfortably in the agent's own context;
/// burning a side-LLM call to summarise <2 KB of text is almost always
/// wasted spend, and the raw markdown is more accurate than any
/// summary the side model would write. Measured in chars (not bytes)
/// so the threshold tracks rough token count consistently across
/// scripts.
const SUMMARY_MIN_CHARS: usize = 2048;
const ERROR_BODY_PREVIEW_BYTES: usize = 8 * 1024;
/// Cap on the rendered-body preview persisted into the trace's
/// `HttpFetch` event payload. The full rendered text still flows back
/// to the agent via `ToolOutput::Text`; this only governs what shows
/// up in the audit trail. Bytes (not chars) — `truncate_utf8` cuts on
/// a UTF-8 boundary.
const HTTP_BODY_PREVIEW_BYTES: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REDIRECT_LIMIT: usize = 5;
const CALL_LABEL_MAX: usize = 120;

const SUMMARY_SYSTEM_PROMPT: &str = "You are extracting information from a fetched web page. \
The user supplies a prompt describing what they want to know, followed by the article text \
extracted from the page (chrome and scripts already stripped). Respond with a concise extraction \
or summary that directly answers the prompt. Quote short fragments verbatim when accuracy \
matters; do not invent details. If the page does not contain the requested information, say so \
explicitly.";

fn host_to_literal_ip(host: &str) -> Option<IpAddr> {
    if let Ok(addr) = host.parse::<IpAddr>() {
        return Some(addr);
    }
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .and_then(|s| s.parse::<IpAddr>().ok())
}

fn validate_url_with(s: &str, allow_loopback: bool) -> Result<url::Url, String> {
    let parsed = url::Url::parse(s).map_err(|e| format!("invalid url `{s}`: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!("scheme `{other}` not allowed (use http or https)"));
        }
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?;
    let host_lc = host.to_ascii_lowercase();
    if host_lc.is_empty() {
        return Err("empty host".to_string());
    }
    if !allow_loopback
        && (host_lc == "localhost"
            || host_lc == "localhost.localdomain"
            || host_lc.ends_with(".localhost"))
    {
        return Err(format!("host `{host}` blocked"));
    }
    if let Some(addr) = host_to_literal_ip(&host_lc)
        && aura_security::is_blocked_ip(&addr, allow_loopback)
    {
        return Err(format!("ip `{addr}` blocked"));
    }
    Ok(parsed)
}

struct SafeResolver {
    allow_loopback: bool,
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let host = name.as_str().to_string();
            let lookup = format!("{host}:0");
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host(lookup)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            let safe: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|sa| !aura_security::is_blocked_ip(&sa.ip(), allow_loopback))
                .collect();
            if safe.is_empty() {
                return Err(format!("host `{host}` resolved only to blocked IP ranges").into());
            }
            let iter: Addrs = Box::new(safe.into_iter());
            Ok(iter)
        })
    }
}

pub struct WebFetchTool {
    client: reqwest::Client,
    validator_allow_loopback: bool,
}

impl WebFetchTool {
    fn build(validator_allow_loopback: bool, resolver_allow_loopback: bool) -> Self {
        let validator_lax = validator_allow_loopback;
        let client = reqwest::Client::builder()
            .user_agent(concat!("aura-webfetch/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .dns_resolver(Arc::new(SafeResolver {
                allow_loopback: resolver_allow_loopback,
            }))
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= REDIRECT_LIMIT {
                    return attempt.error(format!("WebFetch: exceeded {REDIRECT_LIMIT} redirects"));
                }
                if let Err(reason) = validate_url_with(attempt.url().as_str(), validator_lax) {
                    return attempt.error(format!("WebFetch: redirect target rejected: {reason}"));
                }
                let original_host = attempt
                    .previous()
                    .first()
                    .and_then(|u| u.host_str())
                    .map(|h| h.to_ascii_lowercase());
                let target_host = attempt.url().host_str().map(|h| h.to_ascii_lowercase());
                if original_host != target_host {
                    let target_url = attempt.url().to_string();
                    return attempt.error(format!(
                        "WebFetch: cross-host redirect to `{target_url}` blocked; \
                         re-issue WebFetch on the new URL so the host change \
                         is visible in the call trace"
                    ));
                }
                attempt.follow()
            }))
            .build()
            .expect("reqwest::Client builder accepts only static config");
        Self {
            client,
            validator_allow_loopback,
        }
    }

    #[cfg(test)]
    fn for_testing() -> Self {
        Self::build(true, true)
    }

    #[cfg(test)]
    fn for_testing_strict_resolver() -> Self {
        Self::build(true, false)
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::build(false, false)
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    url: String,
    #[serde(default)]
    prompt: Option<String>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        r#"
- Fetches content from a specified URL
- Takes a URL and an optional prompt as input
- Fetches the URL content, extracts the article body as plain text (navigation, sidebars, scripts, and styles are removed)
- When a prompt is supplied the extracted text is run through a side LLM that produces a focused answer; otherwise the text is returned verbatim
- Use this tool when you need to retrieve and analyze web content
Usage notes:
  - The URL must be a fully-formed valid URL
  - HTTP URLs will be automatically upgraded to HTTPS
  - This tool is read-only and does not modify any files
  - If no LLM is configured the prompt is ignored and the extracted text is returned
"#
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url":    { "type": "string", "format": "uri", "description": "The http(s) URL to fetch" },
                "prompt": { "type": "string", "description": "Optional extraction prompt: when set, the rendered page is forwarded to a side LLM that returns a focused answer instead of the raw body" }
            },
            "required": ["url"]
        })
    }

    fn max_timeout(&self) -> Duration {
        // Network fetches over slow upstreams routinely take longer
        // than the trait-default 30 s, but we don't want a stuck host
        // to block forever either. 2 minutes is a comfortable upper
        // bound — `connect_timeout` already caps the connect phase at
        // 10 s independently.
        Duration::from_secs(120)
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Three buckets, only the third actually prompts:
        //   1. hostname URL → []. The SSRF resolver is the sole guard;
        //      prompting per fetch is friction without a real win.
        //   2. literal IP that `is_blocked_ip` would reject → []. The
        //      tool will fail at `validate_url_with` before any request
        //      goes out, so prompting is pure noise — the user clicks
        //      approve and still sees an error. The SSRF floor stays
        //      load-bearing; it just doesn't need a UI prompt on top.
        //   3. literal *public* IP → declare `Http`. RFC-range checks
        //      can't tell a routable IP that belongs to internal
        //      infrastructure from a real public service, so put a
        //      human in the loop.
        params
            .get("url")
            .and_then(|v| v.as_str())
            .and_then(|s| url::Url::parse(s).ok())
            .and_then(|u| u.host_str().map(str::to_lowercase))
            .filter(|host| {
                let Some(addr) = host_to_literal_ip(host) else {
                    return false;
                };
                !aura_security::is_blocked_ip(&addr, self.validator_allow_loopback)
            })
            .map(|host| vec![ResourceAccess::Http { host }])
            .unwrap_or_default()
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        params.get("url").and_then(|v| v.as_str()).map(|s| {
            if s.len() <= CALL_LABEL_MAX {
                s.to_string()
            } else {
                let mut cut = CALL_LABEL_MAX;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &s[..cut])
            }
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let parsed = validate_url_with(&p.url, self.validator_allow_loopback)
            .map_err(|e| ToolError::InvalidParams(format!("WebFetch: {e}")))?;
        let host = parsed.host_str().unwrap_or_default().to_string();

        tracing::info!(host = %host, "WebFetch start");

        let response = {
            let _t = start_timer(&ctx.events, "http_request");
            let send_fut = self.client.get(parsed).timeout(ctx.timeout).send();
            tokio::select! {
                _ = ctx.cancellation_token.cancelled() => {
                    return Err(ToolError::Execution("cancelled".into()));
                }
                res = send_fut => match res {
                    Ok(r) => r,
                    Err(e) if e.is_timeout() => {
                        return Err(ToolError::Timeout(format!(
                            "WebFetch exceeded {:?}", ctx.timeout
                        )));
                    }
                    Err(e) => return Err(ToolError::Execution(reqwest_error_chain(&e))),
                }
            }
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            let body = {
                let _t = start_timer(&ctx.events, "read_error_body");
                read_body(response, ERROR_BODY_PREVIEW_BYTES, ctx).await?
            };
            let snippet = truncate_utf8(&body, ERROR_BODY_PREVIEW_BYTES);
            ctx.events.emit(
                "http_response",
                ToolEventPayload::HttpFetch {
                    status: status.as_u16(),
                    bytes: body.len() as u64,
                    content_type: empty_to_none(&content_type),
                    body_preview: empty_to_none(&snippet),
                },
            );
            tracing::info!(
                host = %host,
                status = status.as_u16(),
                "WebFetch http error"
            );
            return Ok(ToolOutput::Error(format!("HTTP {status}: {snippet}")));
        }

        let ct_lower = content_type.to_ascii_lowercase();
        let is_html = ct_lower.contains("text/html") || ct_lower.contains("application/xhtml");
        let is_text = ct_lower.starts_with("text/")
            || ct_lower.contains("application/json")
            || ct_lower.contains("application/xml")
            || ct_lower.contains("+xml")
            || ct_lower.contains("+json");

        if !is_text && !is_html && !ct_lower.is_empty() {
            return Ok(ToolOutput::Error(format!(
                "WebFetch: refusing non-text content-type `{content_type}`"
            )));
        }

        let body = {
            let _t = start_timer(&ctx.events, "read_body");
            read_body(response, MAX_RESPONSE_BYTES, ctx).await?
        };
        let body_bytes_read = body.len();
        let raw_text = String::from_utf8_lossy(&body);

        let rendered = if is_html {
            let _t = start_timer(&ctx.events, "extract_article");
            extract_article(&raw_text, &host)
        } else {
            raw_text.into_owned()
        };

        ctx.events.emit(
            "http_response",
            ToolEventPayload::HttpFetch {
                status: status.as_u16(),
                bytes: body_bytes_read as u64,
                content_type: empty_to_none(&content_type),
                body_preview: empty_to_none(&truncate_utf8(
                    rendered.as_bytes(),
                    HTTP_BODY_PREVIEW_BYTES,
                )),
            },
        );

        // Side-channel summary path: only when the caller asked for one
        // (`prompt` is non-empty), the agent layer bound a billed LLM
        // into `ctx.llm`, AND the rendered content is large enough to
        // be worth summarising. Pages under `SUMMARY_MIN_CHARS` are
        // returned as raw markdown — they fit in the agent's own
        // context, and a side-LLM round-trip would cost tokens for
        // output that is almost always less faithful than the
        // original. Argv-mode boot, tests, and any deployment that
        // hasn't configured a model also get the raw output.
        if let (Some(llm), Some(prompt_text)) = (
            ctx.llm.as_ref(),
            p.prompt.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        ) && rendered.chars().count() > SUMMARY_MIN_CHARS
        {
            let summary_page = truncate_utf8(rendered.as_bytes(), MAX_SUMMARY_INPUT_BYTES);
            let summary_user_text =
                format!("Prompt: {prompt_text}\n\nPage content:\n{summary_page}");
            let summary = {
                let _t = start_timer(&ctx.events, "llm_summary");
                run_summary(llm.as_ref(), &summary_user_text, ctx).await
            };
            return match summary {
                Ok(text) => {
                    let output = truncate_utf8(text.as_bytes(), MAX_OUTPUT_BYTES);
                    ctx.events.emit(
                        "llm_summary",
                        ToolEventPayload::LlmCall {
                            model: llm.model_info().id.clone(),
                            input: summary_user_text.clone(),
                            output: output.clone(),
                        },
                    );
                    tracing::info!(
                        host = %host,
                        status = status.as_u16(),
                        body_bytes_read,
                        summary_input_bytes = summary_user_text.len(),
                        output_bytes = output.len(),
                        "WebFetch done (summarised)"
                    );
                    Ok(ToolOutput::Text(output))
                }
                Err(e) => {
                    tracing::warn!(host = %host, error = %e, "WebFetch summarise failed");
                    Ok(ToolOutput::Error(format!(
                        "WebFetch: side LLM summarisation failed: {e}"
                    )))
                }
            };
        }

        let output = truncate_utf8(rendered.as_bytes(), MAX_OUTPUT_BYTES);
        tracing::info!(
            host = %host,
            status = status.as_u16(),
            body_bytes_read,
            output_bytes = output.len(),
            "WebFetch done"
        );
        Ok(ToolOutput::Text(output))
    }
}

/// Run the side LLM with the fixed extraction system prompt and the user's
/// `(prompt, page)` pair. Honours `ctx.cancellation_token` and `ctx.timeout`
/// so a slow model can't pin the outer deadline. Cost is recorded inside
/// the [`BilledChat::chat`] impl that the agent layer binds into
/// `ctx.llm`, so a successful return here implies the spend already
/// landed in `cost_records`.
async fn run_summary(
    llm: &dyn BilledChat,
    user_text: &str,
    ctx: &ToolContext,
) -> Result<String, String> {
    let request = ChatRequest {
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(SUMMARY_SYSTEM_PROMPT.to_string())],
                from_user: false,
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text(user_text.to_string())],
                from_user: false,
            },
        ],
        temperature: Some(0.0),
        tools: vec![],
    };
    tokio::select! {
        _ = ctx.cancellation_token.cancelled() => {
            Err("cancelled".to_string())
        }
        res = tokio::time::timeout(ctx.timeout, llm.chat(&request)) => {
            match res {
                Err(_) => Err(format!("summary timed out after {:?}", ctx.timeout)),
                Ok(Err(msg)) => Err(msg),
                Ok(Ok(billed)) => {
                    let trimmed = billed.response.content.trim();
                    if trimmed.is_empty() {
                        Err("model returned empty content".to_string())
                    } else {
                        Ok(trimmed.to_string())
                    }
                }
            }
        }
    }
}

async fn read_body(
    mut response: reqwest::Response,
    cap: usize,
    ctx: &ToolContext,
) -> crate::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = ctx.cancellation_token.cancelled() => {
                return Err(ToolError::Execution("cancelled".into()));
            }
            res = response.chunk() => match res {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) if e.is_timeout() => {
                    return Err(ToolError::Timeout(format!(
                        "WebFetch exceeded {:?}", ctx.timeout
                    )));
                }
                Err(e) => return Err(ToolError::Execution(reqwest_error_chain(&e))),
            }
        };
        let remaining = cap.saturating_sub(buf.len());
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            break;
        }
        buf.extend_from_slice(&chunk);
        if buf.len() >= cap {
            break;
        }
    }
    Ok(buf)
}

/// Run dom_smoothie's Readability over the HTML body and return the
/// article text. Title (when non-empty) is prefixed on its own line so
/// the agent has it without re-parsing. `TextMode::Formatted` keeps
/// block boundaries as newlines without injecting markdown markers.
///
/// On failure (list pages, very short bodies, malformed markup that
/// makes the readability heuristic give up), the raw page bytes are
/// returned verbatim — a noisy text dump still beats an empty
/// `ToolOutput::Text`, and scripts in plain text are inert.
fn extract_article(raw_html: &str, host: &str) -> String {
    let cfg = Config {
        text_mode: TextMode::Formatted,
        ..Config::default()
    };
    let mut readability = match Readability::new(raw_html, None, Some(cfg)) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(host = %host, error = %e, "WebFetch readability init failed; returning raw");
            return raw_html.to_string();
        }
    };
    match readability.parse() {
        Ok(article) => {
            let title = article.title.trim();
            let body = article.text_content.trim();
            if title.is_empty() {
                body.to_string()
            } else {
                format!("{title}\n\n{body}")
            }
        }
        Err(e) => {
            tracing::warn!(host = %host, error = %e, "WebFetch readability parse failed; returning raw");
            raw_html.to_string()
        }
    }
}

fn reqwest_error_chain(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut cursor: Option<&dyn std::error::Error> = std::error::Error::source(e);
    while let Some(inner) = cursor {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        cursor = inner.source();
    }
    s
}

fn empty_to_none(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn truncate_utf8(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let total = bytes.len();
    let mut cut = max;
    while cut > 0 && (bytes[cut] & 0b1100_0000) == 0b1000_0000 {
        cut -= 1;
    }
    let elided = total - cut;
    let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    use std::fmt::Write as _;
    let _ = write!(s, "\n... [truncated {elided} bytes, total {total}] ...");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unbilled_chat;
    use aura_llm::{GuardedLlm, LlmCompletion};
    use aura_model::{ChannelType, User};
    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, HeaderValue, StatusCode, header},
        response::{IntoResponse, Response},
        routing::get,
    };
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    fn billed(stub: Arc<dyn LlmCompletion>) -> Arc<dyn BilledChat> {
        unbilled_chat(GuardedLlm::passthrough(stub))
    }

    fn ctx_with_timeout(timeout: Duration) -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout,
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: crate::noop_event_sink(),
            llm: None,
        }
    }

    fn ctx() -> ToolContext {
        ctx_with_timeout(Duration::from_secs(5))
    }

    #[derive(Default)]
    struct RecorderStub {
        entries: parking_lot::Mutex<Vec<(String, aura_trace::ToolEventPayload)>>,
    }

    impl crate::ToolEventSink for RecorderStub {
        fn emit(&self, action: &str, payload: aura_trace::ToolEventPayload) {
            self.entries.lock().push((action.to_string(), payload));
        }
    }

    fn ctx_with_event_sink(sink: Arc<RecorderStub>) -> ToolContext {
        let mut c = ctx();
        c.events = sink as Arc<dyn crate::ToolEventSink>;
        c
    }

    struct TestServer {
        addr: SocketAddr,
        _handle: tokio::task::JoinHandle<()>,
    }

    async fn spawn(app: Router) -> TestServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        TestServer {
            addr,
            _handle: handle,
        }
    }

    fn url_to(server: &TestServer, path: &str) -> String {
        format!("http://{}{path}", server.addr)
    }

    #[test]
    fn validate_url_rejects_blocked_targets() {
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,foo",
            "ftp://example.com/",
            "http://localhost/",
            "http://foo.localhost/",
            "http://127.0.0.1/",
            "http://127.1.2.3/",
            "http://0.0.0.0/",
            "http://169.254.169.254/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[::]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:10.0.0.1]/",
        ] {
            assert!(
                validate_url_with(bad, false).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn validate_url_accepts_public_targets() {
        for good in [
            "http://example.com/",
            "https://example.com/path?q=1",
            "https://1.1.1.1/",
            "http://[2001:db8::1]/",
        ] {
            assert!(
                validate_url_with(good, false).is_ok(),
                "expected `{good}` to be accepted"
            );
        }
    }

    #[test]
    fn schema_describes_url_param() {
        let schema = WebFetchTool::default().parameters_schema();
        assert_eq!(schema["required"], json!(["url"]));
        assert_eq!(schema["properties"]["url"]["type"], "string");
    }

    #[test]
    fn accessed_resources_skips_hostnames() {
        // Hostname URLs declare no access so the executor's approval
        // gate has nothing to prompt on — the SSRF resolver is the
        // sole guard at runtime.
        let tool = WebFetchTool::default();
        let acc = tool.accessed_resources(&json!({ "url": "https://Example.COM/path" }));
        assert!(acc.is_empty(), "got: {acc:?}");
    }

    #[test]
    fn accessed_resources_declares_http_for_literal_ipv4() {
        let tool = WebFetchTool::default();
        let acc = tool.accessed_resources(&json!({ "url": "https://1.2.3.4/path" }));
        assert!(matches!(
            acc.first(),
            Some(ResourceAccess::Http { host }) if host == "1.2.3.4"
        ));
    }

    #[test]
    fn accessed_resources_declares_http_for_literal_ipv6() {
        let tool = WebFetchTool::default();
        let acc = tool.accessed_resources(&json!({ "url": "https://[2001:db8::1]/" }));
        assert!(
            matches!(acc.first(), Some(ResourceAccess::Http { .. })),
            "got: {acc:?}"
        );
    }

    #[test]
    fn host_to_literal_ip_classifies_correctly() {
        assert!(host_to_literal_ip("1.2.3.4").is_some());
        assert!(host_to_literal_ip("::1").is_some());
        assert!(host_to_literal_ip("[2001:db8::1]").is_some());
        assert!(host_to_literal_ip("example.com").is_none());
        assert!(host_to_literal_ip("1.2.3").is_none());
    }

    /// IPs the SSRF floor would reject must NOT trigger approval —
    /// the tool fails at `validate_url_with` before any request goes
    /// out, so prompting is pure noise. Covers literal RFC1918,
    /// loopback (default `WebFetchTool::default()` runs strict), and
    /// the unusual encodings that `url::Url` canonicalizes into
    /// reserved ranges (`http://2130706433/` → `127.0.0.1` etc.).
    #[test]
    fn accessed_resources_skips_blocked_literal_ips() {
        let tool = WebFetchTool::default();
        for url in [
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/",
            "http://127.0.0.1/",
            "http://2130706433/", // → 127.0.0.1
            "http://0x7f000001/", // → 127.0.0.1
            "http://0177.0.0.1/", // → 127.0.0.1
            "http://127.1/",      // → 127.0.0.1
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
        ] {
            let acc = tool.accessed_resources(&json!({ "url": url }));
            assert!(
                acc.is_empty(),
                "{url} resolves into a blocked range; must not prompt approval, got {acc:?}"
            );
        }
    }

    #[test]
    fn call_label_returns_url_truncated() {
        let tool = WebFetchTool::default();
        assert_eq!(
            tool.call_label(&json!({ "url": "https://example.com/" })),
            Some("https://example.com/".into())
        );
        let long = "https://example.com/".to_string() + &"a".repeat(200);
        let label = tool.call_label(&json!({ "url": long })).unwrap();
        assert!(label.ends_with('…'));
        assert!(label.chars().count() <= CALL_LABEL_MAX + 1);
    }

    #[tokio::test]
    async fn missing_url_is_invalid_params() {
        let err = WebFetchTool::default()
            .execute(json!({}), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn blocked_url_is_invalid_params() {
        let err = WebFetchTool::default()
            .execute(json!({ "url": "http://127.0.0.1/" }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(ref m) if m.contains("blocked")));
    }

    #[tokio::test]
    async fn extra_prompt_field_is_accepted() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "ok") }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(
                json!({ "url": url_to(&server, "/"), "prompt": "summarize" }),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert_eq!(s.trim(), "ok");
    }

    /// Build an HTML body that comfortably exceeds dom_smoothie's
    /// `char_threshold` (500) so the readability heuristic actually
    /// commits to a top candidate. Used by the tests that need a real
    /// article-shaped page rather than the fallback path.
    fn article_html(title: &str, body_paragraph: &str) -> String {
        let filler = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
                      tempor incididunt ut labore et dolore magna aliqua. "
            .repeat(8);
        format!(
            r#"<html><head><title>{title}</title></head><body>
                <nav><a href="/x">menu link</a></nav>
                <header>brand header</header>
                <article>
                  <h1>{title}</h1>
                  <p>{body_paragraph}</p>
                  <p>{filler}</p>
                  <p>{filler}</p>
                  <script>window.tracking = true;</script>
                  <style>.x{{color:red}}</style>
                </article>
                <aside>related links</aside>
                <footer>copyright footer</footer>
            </body></html>"#
        )
    }

    #[tokio::test]
    async fn html_body_is_extracted_as_plain_text() {
        let body = article_html("Real Title", "Hello body world");
        let server = spawn(Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body) }
            }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert!(s.contains("Real Title"), "title missing: {s:?}");
        assert!(s.contains("Hello body world"), "body missing: {s:?}");
        // dom_smoothie returns plain text — no markdown markers.
        assert!(!s.contains("# Real Title"), "unexpected md heading: {s:?}");
        assert!(!s.contains("<h1"), "raw html leaked: {s:?}");
    }

    #[tokio::test]
    async fn extracted_text_excludes_chrome_scripts_and_styles() {
        let body = article_html("Real Title", "real body text continues here.");
        let server = spawn(Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body) }
            }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert!(s.contains("Real Title"), "main content missing: {s}");
        assert!(!s.contains("menu link"), "nav leaked: {s}");
        assert!(!s.contains("brand header"), "header leaked: {s}");
        assert!(!s.contains("copyright footer"), "footer leaked: {s}");
        assert!(!s.contains("window.tracking"), "script body leaked: {s}");
        assert!(!s.contains("color:red"), "style body leaked: {s}");
        assert!(!s.contains("related links"), "aside leaked: {s}");
    }

    #[tokio::test]
    async fn timer_metrics_record_phases_for_html() {
        let body = article_html("Phase Title", "phase body for timer metrics.");
        let server = spawn(Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body) }
            }),
        ))
        .await;
        let metrics = Arc::new(RecorderStub::default());
        WebFetchTool::for_testing()
            .execute(
                json!({ "url": url_to(&server, "/") }),
                &ctx_with_event_sink(Arc::clone(&metrics)),
            )
            .await
            .unwrap();
        let phases: Vec<String> = metrics
            .entries
            .lock()
            .iter()
            .map(|(a, _)| a.clone())
            .collect();
        assert!(
            phases.iter().any(|p| p == "http_request"),
            "missing http_request in {phases:?}"
        );
        assert!(
            phases.iter().any(|p| p == "read_body"),
            "missing read_body in {phases:?}"
        );
        assert!(
            phases.iter().any(|p| p == "extract_article"),
            "missing extract_article in {phases:?}"
        );
        assert!(
            !phases.iter().any(|p| p == "llm_summary"),
            "llm_summary recorded without a prompt+LLM: {phases:?}"
        );

        let entries = metrics.entries.lock();
        let http_response = entries
            .iter()
            .find(|(a, _)| a == "http_response")
            .expect("missing http_response entry");
        match &http_response.1 {
            aura_trace::ToolEventPayload::HttpFetch {
                status,
                content_type,
                body_preview,
                ..
            } => {
                assert_eq!(*status, 200);
                assert_eq!(content_type.as_deref(), Some("text/html; charset=utf-8"));
                let preview = body_preview.as_deref().expect("missing body preview");
                assert!(
                    preview.contains("Phase Title"),
                    "preview missing extracted title: {preview:?}"
                );
                assert!(
                    !preview.contains("<h1"),
                    "preview still contains raw html: {preview:?}"
                );
            }
            other => panic!("expected HttpFetch payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timer_metrics_record_llm_summary_when_prompt_set() {
        use aura_llm::test_support::StubLlm;
        use aura_llm::{LlmResponse, TokenUsage};

        let stub = Arc::new(StubLlm::new());
        stub.push_response(LlmResponse {
            content: "ok".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: TokenUsage::default(),
            thinking: None,
        });

        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                let body = format!(
                    "<h1>Hi</h1><p>{}</p>",
                    "lorem ipsum ".repeat(SUMMARY_MIN_CHARS / 6),
                );
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
            }),
        ))
        .await;
        let tool = WebFetchTool::for_testing();
        let llm = billed(stub.clone() as Arc<dyn LlmCompletion>);
        let metrics = Arc::new(RecorderStub::default());
        let mut tctx = ctx_with_event_sink(Arc::clone(&metrics));
        tctx.llm = Some(llm);
        tool.execute(
            json!({ "url": url_to(&server, "/"), "prompt": "what's the title?" }),
            &tctx,
        )
        .await
        .unwrap();
        let phases: Vec<String> = metrics
            .entries
            .lock()
            .iter()
            .map(|(a, _)| a.clone())
            .collect();
        assert!(
            phases.iter().any(|p| p == "llm_summary"),
            "missing llm_summary in {phases:?}"
        );

        let entries = metrics.entries.lock();
        let llm_call = entries
            .iter()
            .find(|(a, p)| {
                a == "llm_summary" && matches!(p, aura_trace::ToolEventPayload::LlmCall { .. })
            })
            .expect("missing LlmCall payload");
        match &llm_call.1 {
            aura_trace::ToolEventPayload::LlmCall {
                model,
                input,
                output,
            } => {
                assert!(!model.is_empty(), "model id should be populated");
                assert!(input.contains("what's the title?"), "input missing prompt");
                assert_eq!(output, "ok");
            }
            other => panic!("expected LlmCall payload, got {other:?}"),
        }
    }

    /// Content under `SUMMARY_MIN_CHARS` skips the side LLM even when a
    /// prompt is supplied: short pages fit in the agent's own context,
    /// and a side-LLM round-trip would cost tokens for output that is
    /// almost always less faithful than the original. The captured
    /// stub asserts no chat call was made.
    #[tokio::test]
    async fn short_content_skips_llm_even_with_prompt() {
        use aura_llm::test_support::StubLlm;

        let stub = Arc::new(StubLlm::new());
        // No `push_response` — a stray call would 500 the test.
        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<h1>Hi</h1><p>short</p>",
                )
            }),
        ))
        .await;
        let tool = WebFetchTool::for_testing();

        let llm = billed(stub.clone() as Arc<dyn LlmCompletion>);
        let mut tctx = ctx();
        tctx.llm = Some(llm);
        let out = tool
            .execute(
                json!({ "url": url_to(&server, "/"), "prompt": "summarise" }),
                &tctx,
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!("expected Text, got: {out:?}")
        };
        // Short pages fail dom_smoothie's char_threshold, so we hit the
        // raw-html fallback. The agent still gets the page contents;
        // what we care about here is that the LLM short-circuit fired.
        assert!(s.contains("Hi"), "page content missing: {s:?}");
        assert!(
            stub.captured_requests().is_empty(),
            "LLM should not be called for short content"
        );
    }

    #[tokio::test]
    async fn plain_text_body_is_returned_verbatim() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "raw <b>text</b>") }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert_eq!(s, "raw <b>text</b>");
    }

    #[tokio::test]
    async fn binary_content_type_is_refused() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    vec![0u8, 1, 2, 3],
                )
            }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Error(s) = out else { panic!() };
        assert!(s.contains("non-text content-type"));
    }

    #[tokio::test]
    async fn http_500_returns_tool_output_error() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "text/plain")],
                    "boom",
                )
                    .into_response()
            }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Error(s) = out else { panic!() };
        assert!(s.starts_with("HTTP 500"));
        assert!(s.contains("boom"));
    }

    #[tokio::test]
    async fn oversize_body_is_truncated() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                let body = "x".repeat(MAX_OUTPUT_BYTES + 4096);
                ([(header::CONTENT_TYPE, "text/plain")], body)
            }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert!(s.ends_with("] ..."), "tail: {:?}", &s[s.len() - 80..]);
        assert!(s.contains("truncated"));
    }

    #[tokio::test]
    async fn timeout_is_reported() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                ([(header::CONTENT_TYPE, "text/plain")], "late")
            }),
        ))
        .await;
        let err = WebFetchTool::for_testing()
            .execute(
                json!({ "url": url_to(&server, "/") }),
                &ctx_with_timeout(Duration::from_millis(80)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn pre_cancelled_token_aborts() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                "late"
            }),
        ))
        .await;
        let c = ctx();
        c.cancellation_token.cancel();
        let err = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &c)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("cancelled")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn redirect_to_blocked_target_fails() {
        #[derive(Clone)]
        struct AppState {
            target: Arc<String>,
        }
        let state = AppState {
            target: Arc::new("http://10.0.0.1:1/".to_string()),
        };
        let server = spawn(
            Router::new()
                .route(
                    "/",
                    get(|State(s): State<AppState>| async move {
                        let mut h = HeaderMap::new();
                        h.insert(header::LOCATION, HeaderValue::from_str(&s.target).unwrap());
                        (StatusCode::FOUND, h, "").into_response()
                    }),
                )
                .with_state(state),
        )
        .await;
        let err = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn redirect_to_public_target_is_followed() {
        #[derive(Clone)]
        struct AppState {
            counter: Arc<AtomicUsize>,
        }
        async fn root(State(s): State<AppState>) -> Response {
            s.counter.fetch_add(1, Ordering::SeqCst);
            let mut h = HeaderMap::new();
            h.insert(header::LOCATION, HeaderValue::from_static("/dst"));
            (StatusCode::FOUND, h, "").into_response()
        }
        async fn dst() -> ([(header::HeaderName, &'static str); 1], &'static str) {
            ([(header::CONTENT_TYPE, "text/plain")], "final")
        }
        let state = AppState {
            counter: Arc::new(AtomicUsize::new(0)),
        };
        let server = spawn(
            Router::new()
                .route("/", get(root))
                .route("/dst", get(dst))
                .with_state(state.clone()),
        )
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert_eq!(s, "final");
        assert_eq!(state.counter.load(Ordering::SeqCst), 1);
    }

    /// Finding 1 (DNS-aware SSRF): the validator allows `localhost`
    /// through (lax-validator config), but the resolver runs
    /// `is_blocked_ip` over every resolved address and rejects loopback
    /// under the strict `resolver_allow_loopback = false` posture used in
    /// production. (Literal-IP URLs bypass the DNS resolver in reqwest's
    /// connector, so a hostname is required to exercise this path.)
    #[tokio::test]
    async fn resolver_rejects_dns_resolved_to_blocked_ip() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "should-not-arrive") }),
        );
        let _handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://localhost:{}/", addr.port());
        let err = WebFetchTool::for_testing_strict_resolver()
            .execute(json!({ "url": url }), &ctx())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            matches!(err, ToolError::Execution(_)) && msg.to_lowercase().contains("blocked"),
            "got: {err:?}"
        );
    }

    /// Finding 2 (cross-host redirect): a redirect to a different host that
    /// would otherwise pass SSRF validation must still be rejected so the
    /// LLM has to re-issue WebFetch with the new host visible in the call
    /// trace. Uses 127.0.0.1 → localhost (different host strings, same
    /// loopback IP, both lax-allowed) to isolate the cross-host check
    /// from the SSRF resolver.
    #[tokio::test]
    async fn cross_host_redirect_is_blocked_even_when_target_is_safe() {
        #[derive(Clone)]
        struct AppState {
            target: Arc<String>,
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let target = format!("http://localhost:{}/", addr.port());
        let state = AppState {
            target: Arc::new(target),
        };
        let app = Router::new()
            .route(
                "/",
                get(|State(s): State<AppState>| async move {
                    let mut h = HeaderMap::new();
                    h.insert(header::LOCATION, HeaderValue::from_str(&s.target).unwrap());
                    (StatusCode::FOUND, h, "").into_response()
                }),
            )
            .with_state(state);
        let _handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://127.0.0.1:{}/", addr.port());
        let err = WebFetchTool::for_testing()
            .execute(json!({ "url": url }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("cross-host")),
            "got: {err:?}"
        );
    }

    /// Side-LLM summary path: when both `prompt` and an `LlmCompletion`
    /// are present, the rendered markdown is replaced by the model's
    /// reply. The captured request is asserted to confirm the page
    /// content actually flowed through.
    #[tokio::test]
    async fn prompt_with_llm_returns_summary() {
        use aura_llm::test_support::StubLlm;
        use aura_llm::{LlmResponse, TokenUsage};

        let stub = Arc::new(StubLlm::new());
        stub.push_response(LlmResponse {
            content: "Title is `Hello`.".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: TokenUsage::default(),
            thinking: None,
        });

        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                let body = format!(
                    "<h1>Hello</h1><p>{}</p>",
                    "lorem ipsum ".repeat(SUMMARY_MIN_CHARS / 6),
                );
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
            }),
        ))
        .await;

        let tool = WebFetchTool::for_testing();

        let llm = billed(stub.clone() as Arc<dyn LlmCompletion>);
        let mut tctx = ctx();
        tctx.llm = Some(llm);
        let out = tool
            .execute(
                json!({ "url": url_to(&server, "/"), "prompt": "what's the title?" }),
                &tctx,
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!("expected Text, got: {out:?}")
        };
        assert_eq!(s, "Title is `Hello`.");

        let captured = stub.captured_requests();
        assert_eq!(captured.len(), 1, "exactly one chat call");
        let req = &captured[0];
        // System prompt for extraction.
        assert!(matches!(
            req.messages.first().map(|m| m.role),
            Some(aura_model::Role::System)
        ));
        // User message carries both the prompt and the rendered markdown.
        let user = req.messages.last().expect("user msg");
        assert!(matches!(user.role, aura_model::Role::User));
        let aura_model::ContentBlock::Text(user_text) = user.content.first().expect("text block")
        else {
            panic!("expected text block")
        };
        assert!(user_text.contains("what's the title?"));
        assert!(
            user_text.contains("Hello"),
            "extracted title missing: {user_text}"
        );
        assert!(
            !user_text.contains("<h1"),
            "raw html leaked to LLM: {user_text}"
        );
    }

    /// Without a prompt the LLM is *not* called even when one is wired
    /// in — the extracted text still wins. Guards against spending
    /// tokens on every fetch.
    #[tokio::test]
    async fn no_prompt_skips_llm_even_when_wired() {
        use aura_llm::test_support::StubLlm;

        let stub = Arc::new(StubLlm::new());
        // Intentionally no `push_response`: a stray call would 500.
        let body = article_html("Plain Title", "the body of an article goes here.");
        let server = spawn(Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body) }
            }),
        ))
        .await;
        let tool = WebFetchTool::for_testing();

        let llm = billed(stub.clone() as Arc<dyn LlmCompletion>);
        let mut tctx = ctx();
        tctx.llm = Some(llm);
        let out = tool
            .execute(json!({ "url": url_to(&server, "/") }), &tctx)
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!("expected Text, got: {out:?}")
        };
        assert!(s.contains("Plain Title"), "title missing: {s:?}");
        assert!(stub.captured_requests().is_empty());
    }

    /// Empty/whitespace-only prompts are equivalent to no prompt — same
    /// short-circuit to extracted text, no LLM call.
    #[tokio::test]
    async fn whitespace_prompt_skips_llm() {
        use aura_llm::test_support::StubLlm;

        let stub = Arc::new(StubLlm::new());
        let server = spawn(Router::new().route(
            "/",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "ok") }),
        ))
        .await;
        let tool = WebFetchTool::for_testing();

        let llm = billed(stub.clone() as Arc<dyn LlmCompletion>);
        let mut tctx = ctx();
        tctx.llm = Some(llm);
        let out = tool
            .execute(
                json!({ "url": url_to(&server, "/"), "prompt": "   \t\n  " }),
                &tctx,
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!("expected Text, got: {out:?}")
        };
        assert_eq!(s.trim(), "ok");
        assert!(stub.captured_requests().is_empty());
    }

    /// LLM error is surfaced as `ToolOutput::Error` rather than a hard
    /// `ToolError`, so the caller can see the failure reason and retry
    /// (e.g. without `prompt`) instead of having the call disappear into
    /// the timeout/cancel buckets.
    #[tokio::test]
    async fn llm_error_returns_tool_output_error() {
        use aura_llm::LlmError;
        use aura_llm::test_support::StubLlm;

        let stub = Arc::new(StubLlm::new());
        stub.push_response_err(LlmError::Transient("boom".into()));

        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                let body = "page text ".repeat(SUMMARY_MIN_CHARS / 10 + 32);
                ([(header::CONTENT_TYPE, "text/plain")], body)
            }),
        ))
        .await;
        let tool = WebFetchTool::for_testing();

        let llm = billed(stub.clone() as Arc<dyn LlmCompletion>);
        let mut tctx = ctx();
        tctx.llm = Some(llm);
        let out = tool
            .execute(json!({ "url": url_to(&server, "/"), "prompt": "?" }), &tctx)
            .await
            .unwrap();
        let ToolOutput::Error(s) = out else {
            panic!("expected Error, got: {out:?}")
        };
        assert!(s.contains("summarisation failed"), "got: {s}");
        assert!(s.contains("boom"), "got: {s}");
    }
}
