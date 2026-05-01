//! `WebFetch` — GET an `http(s)` URL, render the body as Markdown, return text.
//!
//! Matches Claude Code's WebFetch tool shape: `{ url, prompt? }`. The `prompt`
//! field is accepted for API compatibility but currently ignored — `ToolContext`
//! does not expose an LLM handle, so server-side summarization is deferred.
//! `docs/modules/tools.md` records this gap as "returns raw body; no
//! side-channel LLM extraction yet".
//!
//! Output is capped at 256 KiB on a UTF-8 char boundary, matching the per-tool
//! cap documented in `docs/modules/security.md`. The gateway applies a final
//! cap on top of this; injection scanning and `<tool_output>` wrapping are also
//! the gateway's job — this tool just returns the raw text.
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
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const ERROR_BODY_PREVIEW_BYTES: usize = 8 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REDIRECT_LIMIT: usize = 5;
const CALL_LABEL_MAX: usize = 120;

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
    #[allow(dead_code)]
    prompt: Option<String>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over HTTP(S) and return its body. HTML responses are \
         converted to Markdown; other text/* responses are returned verbatim; \
         binary responses are refused.\n\n\
         Output is capped at 256 KiB on a UTF-8 boundary; longer bodies are \
         truncated with a marker. Redirects are followed up to 5 hops.\n\n\
         Blocked by an SSRF floor: only `http` / `https` schemes are allowed, \
         and literal-IP hosts in loopback (127/8, ::1), private (10/8, \
         172.16/12, 192.168/16, 100.64/10), link-local (169.254/16, fe80::/10 \
         — covers the AWS metadata IP), unique-local (fc00::/7), unspecified \
         (0.0.0.0, ::), and IPv4-mapped-v6 ranges are rejected.\n\n\
         The optional `prompt` parameter is accepted for forward \
         compatibility but currently ignored — the body is returned as-is."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url":    { "type": "string", "format": "uri", "description": "The http(s) URL to fetch" },
                "prompt": { "type": "string", "description": "Currently ignored; reserved for future LLM-side extraction" }
            },
            "required": ["url"]
        })
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
            .map_err(ToolError::InvalidParams)?;
        let host = parsed.host_str().unwrap_or_default().to_string();

        tracing::info!(host = %host, "WebFetch start");

        let send_fut = self.client.get(parsed).timeout(ctx.timeout).send();
        let response = tokio::select! {
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
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            let body = read_body(response, ERROR_BODY_PREVIEW_BYTES, ctx).await?;
            let snippet = truncate_utf8(&body, ERROR_BODY_PREVIEW_BYTES);
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

        let body = read_body(response, MAX_RESPONSE_BYTES, ctx).await?;
        let body_bytes_read = body.len();
        let raw_text = String::from_utf8_lossy(&body);

        let rendered = if is_html {
            match htmd::convert(&raw_text) {
                Ok(md) => md,
                Err(e) => {
                    tracing::warn!(host = %host, error = %e, "WebFetch html2md failed; returning raw");
                    raw_text.into_owned()
                }
            }
        } else {
            raw_text.into_owned()
        };

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
            return Err(format!(
                "WebFetch: scheme `{other}` not allowed (use http or https)"
            ));
        }
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "WebFetch: url has no host".to_string())?;
    let host_lc = host.to_ascii_lowercase();
    if host_lc.is_empty() {
        return Err("WebFetch: empty host".to_string());
    }
    if !allow_loopback
        && (host_lc == "localhost"
            || host_lc == "localhost.localdomain"
            || host_lc.ends_with(".localhost"))
    {
        return Err(format!("WebFetch: host `{host}` blocked"));
    }
    if let Some(addr) = host_to_literal_ip(&host_lc)
        && aura_security::is_blocked_ip(&addr, allow_loopback)
    {
        return Err(format!("WebFetch: ip `{addr}` blocked"));
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
                return Err(
                    format!("WebFetch: host `{host}` resolved only to blocked IP ranges").into(),
                );
            }
            let iter: Addrs = Box::new(safe.into_iter());
            Ok(iter)
        })
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

    fn ctx_with_timeout(timeout: Duration) -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            bot_id: None,
            },
            timeout,
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            sandbox: None,
            approval: None,
        }
    }

    fn ctx() -> ToolContext {
        ctx_with_timeout(Duration::from_secs(5))
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

    #[tokio::test]
    async fn html_body_is_converted_to_markdown() {
        let server = spawn(Router::new().route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<h1>Title</h1><p>Hello <b>world</b></p>",
                )
            }),
        ))
        .await;
        let out = WebFetchTool::for_testing()
            .execute(json!({ "url": url_to(&server, "/") }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert!(s.contains("# Title"), "missing markdown heading: {s:?}");
        assert!(s.contains("**world**"), "missing markdown bold: {s:?}");
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
}
