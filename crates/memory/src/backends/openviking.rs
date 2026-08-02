//! OpenViking memory backend.
//!
//! Self-hosted context database (Volcengine / ByteDance) that organizes agent
//! knowledge into a filesystem hierarchy under `viking://` URIs with tiered
//! context loading (abstract / overview / full) and server-side memory
//! extraction.
//!
//! **Automatic hooks** (the core's recall / write path):
//! - **`recall`**: `POST /api/v1/search/find` with `{query, top_k}`.
//! - **`on_turn_complete`**: `POST /api/v1/sessions/{ctx.session_id}/messages`
//!   ×2 (user + assistant). The server accumulates session state for the
//!   eventual commit.
//! - **`on_session_end`**: `POST /api/v1/sessions/{ctx.session_id}/commit`,
//!   skipped when `transcript.is_empty()`. Triggers the 6-category server-side
//!   extraction (preferences / entities / events / cases / patterns / profile).
//!
//! **Tools** (the model's explicit-signal path) — four `viking_`-prefixed tools
//! ported from the official OpenViking `openclaw-plugin`:
//!
//! | Tool | Mechanism |
//! | --- | --- |
//! | `viking_recall` | `find` across `viking://user/memories` + `viking://agent/memories`, merge / dedup / leaf-filter. |
//! | `viking_store` | write one session message, `commit`, then poll the extraction task. |
//! | `viking_forget` | delete by URI (`DELETE /api/v1/fs`), or search-and-delete on a strong single match. |
//! | `viking_archive_expand` | fetch original messages from a compressed session archive. |
//!
//! # References
//!
//! - OpenViking project: <https://github.com/volcengine/OpenViking>
//! - Tool surface ported from the official OpenViking `openclaw-plugin`
//!   (`examples/openclaw-plugin`): tools `memory_recall` / `memory_store` /
//!   `memory_forget` / `ov_archive_expand` (here `viking_`-prefixed) and the
//!   REST endpoint map in its `client.ts`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use async_trait::async_trait;
use baybo_model::{ChatMessage, ContentBlock, TrustLevel};
use baybo_security::http::ProxySettings;
use baybo_security::{SecretVault, USER_SECRET_PREFIX};
use baybo_tools::{
    Tool, ToolCapability, ToolContext, ToolError, ToolEventSink, ToolManifest, ToolOutput,
};
use baybo_trace::ToolEventPayload;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{Memory, MemoryContext, MemoryError, RecalledMemory, Result};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:1933";
const DEFAULT_ACCOUNT: &str = "default";
/// Agent partition for the account-level `/health` probe, which belongs to
/// no session. The built-in profile's id, so the probe looks like ordinary
/// built-in traffic rather than minting a namespace of its own.
const PROBE_AGENT: &str = baybo_model::BUILTIN_AGENT_PROFILE_ID;
const DEFAULT_TOP_K: usize = 5;
/// Per-request timeout budgets. Production uses [`OpenVikingTimeouts::default`];
/// tests inject ms-scale values via [`OpenVikingMemory::with_timeouts`] to
/// exercise the timeout code paths without real-time sleeps.
#[derive(Debug, Clone)]
pub struct OpenVikingTimeouts {
    /// Default per-request budget for tool calls (model is already waiting).
    pub http: Duration,
    /// `GET /health` probe budget.
    pub health: Duration,
    /// Recall is on the critical path — cap it well below `http` so a slow /
    /// down OpenViking server degrades the turn to "no recalled context"
    /// instead of stalling the user up to the full `http` budget.
    pub recall: Duration,
    /// Background-write budget for `on_turn_complete` / `on_session_end`.
    /// Detached on the runtime root, so the user never waits;
    /// `/sessions/{sid}/commit` triggers the 6-category server-side extraction
    /// (LLM-backed), which can be slow under load — give it a generous ceiling.
    pub write: Duration,
    /// `viking_store` polls the commit's extraction task up to this ceiling.
    pub store_poll: Duration,
    /// `viking_store`'s declared wall-clock (`Tool::max_timeout`).
    pub store_max: Duration,
}

impl Default for OpenVikingTimeouts {
    fn default() -> Self {
        Self {
            http: Duration::from_secs(30),
            health: Duration::from_secs(3),
            recall: Duration::from_secs(5),
            write: Duration::from_secs(600),
            store_poll: Duration::from_secs(110),
            store_max: Duration::from_secs(120),
        }
    }
}
/// Default user-secret name (managed via `baybo secret add`) holding the
/// OpenViking API key. Doubles as the process-env var name. Optional —
/// when nothing is set the backend runs unauthenticated (local-dev mode).
const DEFAULT_API_KEY_NAME: &str = "OPENVIKING_API_KEY";

pub const TOOL_RECALL: &str = "viking_recall";
pub const TOOL_STORE: &str = "viking_store";
pub const TOOL_FORGET: &str = "viking_forget";
pub const TOOL_ARCHIVE_EXPAND: &str = "viking_archive_expand";

/// The two memory roots `viking_recall` queries when no `targetUri` is given —
/// mirrors the official plugin's dual user + agent recall.
const USER_MEMORIES_URI: &str = "viking://user/memories";
const AGENT_MEMORIES_URI: &str = "viking://agent/memories";
/// Default minimum score for `viking_recall` / `viking_forget` candidate
/// filtering (mirrors the plugin's `recallScoreThreshold`).
const DEFAULT_SCORE_THRESHOLD: f64 = 0.15;
/// `viking_forget` search-and-delete: how many candidates to pull, and the
/// single-match score above which deletion happens without confirmation.
const FORGET_DEFAULT_LIMIT: usize = 5;
const FORGET_AUTO_DELETE_SCORE: f64 = 0.85;
/// `viking_store` polls the commit's Phase-2 extraction task at this cadence;
/// the ceiling is [`OpenVikingTimeouts::store_poll`].
const STORE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Body preview cap shared by the tracing logs and the `HttpFetch`
/// trace event payload. Wide enough to be useful for debugging, short
/// enough to keep logs / traces compact.
const HTTP_PREVIEW_BYTES: usize = 512;

fn preview_value(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    truncate(&s, HTTP_PREVIEW_BYTES)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…[{} bytes truncated]", &s[..end], s.len() - end)
    }
}

/// Per-backend config deserialized from `MemoryConfig.extra`. Unset fields
/// elide from JSON via `skip_serializing_if`, so a freshly-written `extra`
/// is `{}` instead of `{"endpoint": null, "api_key_name": null, ...}`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OpenVikingConfig {
    /// Override the OpenViking REST endpoint. `None` →
    /// `http://127.0.0.1:1933` (local dev default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Name of the user secret holding the OpenViking API key. Resolution:
    /// vault entry `user_env.<api_key_name>` (managed via
    /// `baybo secret add <name>`), then process-env `<api_key_name>`.
    /// `None` defaults the name to `"OPENVIKING_API_KEY"`. Leave the
    /// secret itself empty for unauthenticated local-dev servers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_name: Option<String>,
    /// `X-OpenViking-Account` header (tenant identity). Per-user scope is the
    /// `MemoryContext::user_id()` at each call, sent as `X-OpenViking-User`.
    /// `None` → `"default"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Max results returned by `recall`. `None` → 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
}

impl OpenVikingConfig {
    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(DEFAULT_ENDPOINT)
    }
    fn account(&self) -> &str {
        self.account.as_deref().unwrap_or(DEFAULT_ACCOUNT)
    }
    fn top_k(&self) -> usize {
        self.top_k.unwrap_or(DEFAULT_TOP_K)
    }
}

/// Resolve the OpenViking API key. Order:
///   1. User secret vault entry `user_env.<name>` (managed via
///      `baybo secret add <name>`).
///   2. Process env var of the same name.
///
/// `<name>` is `cfg.api_key_name` or [`DEFAULT_API_KEY_NAME`] when unset.
/// Returns an empty string when nothing is found — OpenViking local-dev
/// mode runs without authentication.
pub async fn resolve_api_key(cfg: &OpenVikingConfig, vault: Option<&SecretVault>) -> String {
    let name = cfg.api_key_name.as_deref().unwrap_or(DEFAULT_API_KEY_NAME);
    if let Some(vault) = vault {
        let key = format!("{USER_SECRET_PREFIX}{name}");
        if let Ok(Some(secret)) = vault.get_secret(&key).await
            && let Ok(s) = secret.as_str()
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    std::env::var(name).unwrap_or_default()
}

/// The identity every OpenViking request carries: which human, and which
/// agent partition. One value so the two can never be passed in the wrong
/// order, and so threading them does not push call signatures past their
/// argument budget.
#[derive(Clone, Copy)]
struct VikingScope<'a> {
    user: &'a str,
    agent: &'a str,
}

impl<'a> VikingScope<'a> {
    fn new(user: &'a str, agent: &'a str) -> Self {
        Self { user, agent }
    }
}

struct OpenVikingInner {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    account: String,
    top_k: usize,
    timeouts: OpenVikingTimeouts,
}

impl OpenVikingInner {
    fn url(&self, path: &str) -> String {
        let base = self.endpoint.trim_end_matches('/');
        format!("{base}{path}")
    }

    fn base_headers(&self, scope: VikingScope<'_>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&self.account) {
            h.insert(HeaderName::from_static("x-openviking-account"), v);
        }
        if let Ok(v) = HeaderValue::from_str(scope.user) {
            h.insert(HeaderName::from_static("x-openviking-user"), v);
        }
        if let Ok(v) = HeaderValue::from_str(scope.agent) {
            h.insert(HeaderName::from_static("x-openviking-agent"), v);
        }
        if !self.api_key.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&self.api_key) {
                h.insert(HeaderName::from_static("x-api-key"), v.clone());
            }
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", self.api_key)) {
                h.insert(AUTHORIZATION, v);
            }
        }
        h
    }

    fn json_headers(&self, scope: VikingScope<'_>) -> HeaderMap {
        let mut h = self.base_headers(scope);
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h
    }

    fn parse_body(&self, status: reqwest::StatusCode, body: &str) -> Result<Value> {
        let parsed: Option<Value> = serde_json::from_str(body).ok();
        if !status.is_success() {
            if let Some(v) = &parsed
                && let Some(err) = v.get("error")
            {
                let code = err
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("HTTP_ERROR");
                let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
                return Err(MemoryError::Backend(format!("openviking {code}: {msg}")));
            }
            return Err(MemoryError::Backend(format!(
                "openviking returned {status}: {body}"
            )));
        }
        if let Some(v) = &parsed
            && v.get("status").and_then(|s| s.as_str()) == Some("error")
        {
            return Err(MemoryError::Backend(format!("openviking: {v}")));
        }
        Ok(parsed.unwrap_or(Value::Null))
    }

    /// Drive a prepared `RequestBuilder` and emit observability:
    /// - `tracing::debug!` with the request preview before the call.
    /// - `tracing::info!` with elapsed / status / response preview after.
    /// - When `events` is `Some` (the tool-call path), an `HttpFetch`
    ///   trace event lands on the surrounding span.
    async fn run_request(
        &self,
        method: &str,
        path: &str,
        builder: reqwest::RequestBuilder,
        request_preview: Option<String>,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<Value> {
        let started = Instant::now();
        if let Some(preview) = &request_preview {
            debug!(
                backend = "openviking",
                method, path,
                request = %preview,
                "openviking outbound request"
            );
        }
        let resp = match builder.send().await {
            Ok(r) => r,
            Err(e) => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                info!(
                    backend = "openviking",
                    method, path, elapsed_ms,
                    error = %e,
                    "openviking HTTP send failed"
                );
                return Err(MemoryError::Backend(format!(
                    "openviking {method} {path} failed: {e}"
                )));
            }
        };
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let body_text = resp.text().await.unwrap_or_default();
        let bytes = body_text.len() as u64;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        info!(
            backend = "openviking",
            method, path,
            status = %status,
            elapsed_ms, bytes,
            response = %truncate(&body_text, HTTP_PREVIEW_BYTES),
            "openviking HTTP roundtrip"
        );
        if let Some(sink) = events {
            sink.emit(
                "openviking_http",
                ToolEventPayload::HttpFetch {
                    status: status.as_u16(),
                    bytes,
                    content_type,
                    body_preview: Some(truncate(&body_text, HTTP_PREVIEW_BYTES)),
                },
            );
        }
        self.parse_body(status, &body_text)
    }

    async fn get(
        &self,
        path: &str,
        scope: VikingScope<'_>,
        query: &[(&str, &str)],
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<Value> {
        let url = build_url(&self.endpoint, path, query)
            .map_err(|e| MemoryError::Backend(format!("openviking url build failed: {e}")))?;
        let builder = self
            .client
            .get(url)
            .headers(self.base_headers(scope))
            .timeout(timeout);
        let preview = if query.is_empty() {
            None
        } else {
            Some(format!("{query:?}"))
        };
        self.run_request("GET", path, builder, preview, events)
            .await
    }

    async fn post_json(
        &self,
        path: &str,
        scope: VikingScope<'_>,
        body: &Value,
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<Value> {
        let builder = self
            .client
            .post(self.url(path))
            .headers(self.json_headers(scope))
            .json(body)
            .timeout(timeout);
        self.run_request("POST", path, builder, Some(preview_value(body)), events)
            .await
    }

    async fn post_empty(
        &self,
        path: &str,
        scope: VikingScope<'_>,
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<Value> {
        let builder = self
            .client
            .post(self.url(path))
            .headers(self.json_headers(scope))
            .body("{}")
            .timeout(timeout);
        self.run_request("POST", path, builder, Some("{}".into()), events)
            .await
    }

    async fn delete(
        &self,
        path: &str,
        scope: VikingScope<'_>,
        query: &[(&str, &str)],
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<Value> {
        let url = build_url(&self.endpoint, path, query)
            .map_err(|e| MemoryError::Backend(format!("openviking url build failed: {e}")))?;
        let builder = self
            .client
            .delete(url)
            .headers(self.base_headers(scope))
            .timeout(timeout);
        let preview = if query.is_empty() {
            None
        } else {
            Some(format!("{query:?}"))
        };
        self.run_request("DELETE", path, builder, preview, events)
            .await
    }

    /// `POST /sessions/{session_id}/commit`, parsed into a [`CommitAck`].
    async fn commit(
        &self,
        scope: VikingScope<'_>,
        session_id: &str,
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<CommitAck> {
        let path = format!("/api/v1/sessions/{session_id}/commit");
        let resp = self.post_empty(&path, scope, timeout, events).await?;
        Ok(CommitAck {
            status: resp
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            task_id: resp
                .get("task_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            memories_extracted: count_extracted(&resp),
        })
    }

    /// Poll `/tasks/{task_id}` until terminal, `timeout` elapses, or
    /// `cancelled()` returns true. A poll `get` error ends it early. Shared by
    /// `viking_store` and the public [`OpenVikingMemory::wait_commit_task`].
    async fn poll_commit_task(
        &self,
        scope: VikingScope<'_>,
        task_id: &str,
        interval: Duration,
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
        cancelled: impl Fn() -> bool,
    ) -> CommitTaskOutcome {
        let task_path = format!("/api/v1/tasks/{task_id}");
        let deadline = Instant::now() + timeout;
        let mut outcome = CommitTaskOutcome {
            status: "pending".to_string(),
            memories_extracted: 0,
        };
        loop {
            if cancelled() {
                outcome.status = "cancelled".to_string();
                return outcome;
            }
            if Instant::now() >= deadline {
                outcome.status = "timeout".to_string();
                return outcome;
            }
            tokio::time::sleep(interval).await;
            match self
                .get(&task_path, scope, &[], self.timeouts.http, events)
                .await
            {
                Ok(task) => match task.get("status").and_then(|v| v.as_str()) {
                    Some("completed") => {
                        outcome.status = "completed".to_string();
                        if let Some(result) = task.get("result") {
                            outcome.memories_extracted = count_extracted(result);
                        }
                        return outcome;
                    }
                    Some("failed") => {
                        outcome.status = "failed".to_string();
                        return outcome;
                    }
                    _ => {}
                },
                Err(e) => {
                    debug!(error = %e, "openviking commit task poll failed");
                    outcome.status = "error".to_string();
                    return outcome;
                }
            }
        }
    }
}

fn build_url(endpoint: &str, path: &str, query: &[(&str, &str)]) -> Result<url::Url> {
    let base = endpoint.trim_end_matches('/');
    let raw = format!("{base}{path}");
    let mut url = url::Url::parse(&raw)
        .map_err(|e| MemoryError::Backend(format!("invalid URL {raw}: {e}")))?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query {
            pairs.append_pair(k, v);
        }
    }
    Ok(url)
}

/// Parsed `/sessions/{sid}/commit` response. A commit kicks off the 6-category
/// server-side extraction; when that runs asynchronously the ack carries a
/// `task_id` to poll via [`OpenVikingMemory::wait_commit_task`], otherwise it
/// is absent (extraction completed synchronously). `memories_extracted` is the
/// count the commit itself already reports (0 while a task is still running).
#[derive(Debug, Clone)]
pub struct CommitAck {
    pub status: String,
    pub task_id: Option<String>,
    pub memories_extracted: u64,
}

/// Terminal outcome of polling a commit's extraction task. `status` is one of
/// `completed` / `failed` / `timeout` / `cancelled` / `error`; only `completed`
/// carries a meaningful `memories_extracted`.
#[derive(Debug, Clone)]
pub struct CommitTaskOutcome {
    pub status: String,
    pub memories_extracted: u64,
}

/// OpenViking memory backend. Construct via [`OpenVikingMemory::new`].
///
/// Unlike `mem0` this backend ships no circuit breaker (matches the original
/// reference) — the default endpoint is loopback / local dev, where a
/// dropped server during iteration would otherwise pin the breaker open for
/// the cooldown window. Failures are logged at `warn`/`debug` and swallowed
/// per call.
pub struct OpenVikingMemory {
    inner: Arc<OpenVikingInner>,
}

impl OpenVikingMemory {
    /// Build the backend. `proxy` is threaded into the underlying
    /// `reqwest::Client` via [`baybo_security::http::client_builder`] — the
    /// crate-wide outbound-egress chokepoint — so a deployment-configured
    /// proxy applies to recall/write/tool traffic. `ALWAYS_DIRECT`
    /// (`localhost,127.0.0.1,::1`) is honoured, so the default loopback
    /// endpoint stays direct.
    pub fn new(
        cfg: OpenVikingConfig,
        api_key: String,
        proxy: Option<&ProxySettings>,
    ) -> Result<Self> {
        Self::build(cfg, api_key, proxy, OpenVikingTimeouts::default())
    }

    /// Like [`Self::new`] but with caller-supplied timeout budgets. Production
    /// uses [`Self::new`] (default budgets); callers that need non-default
    /// timeouts — diagnostics, tests exercising the timeout paths without
    /// real-time sleeps — go through here.
    pub fn with_timeouts(
        cfg: OpenVikingConfig,
        api_key: String,
        proxy: Option<&ProxySettings>,
        timeouts: OpenVikingTimeouts,
    ) -> Result<Self> {
        Self::build(cfg, api_key, proxy, timeouts)
    }

    fn build(
        cfg: OpenVikingConfig,
        api_key: String,
        proxy: Option<&ProxySettings>,
        timeouts: OpenVikingTimeouts,
    ) -> Result<Self> {
        let client = baybo_security::http::client_builder(proxy)
            .map_err(|e| MemoryError::Backend(format!("openviking client build failed: {e}")))?
            .timeout(timeouts.http)
            .build()
            .map_err(|e| MemoryError::Backend(format!("openviking client build failed: {e}")))?;
        let inner = Arc::new(OpenVikingInner {
            client,
            endpoint: cfg.endpoint().trim_end_matches('/').to_string(),
            api_key,
            account: cfg.account().to_string(),
            top_k: cfg.top_k(),
            timeouts,
        });
        Ok(Self { inner })
    }

    /// Best-effort `GET /health` probe. Logs `warn` on failure; the runtime
    /// keeps the backend registered.
    pub async fn probe(&self) {
        let result = self
            .inner
            .client
            .get(self.inner.url("/health"))
            .headers(
                self.inner
                    .base_headers(VikingScope::new("default", PROBE_AGENT)),
            )
            .timeout(self.inner.timeouts.health)
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => warn!(
                status = %resp.status(),
                "openviking health probe returned non-success; continuing"
            ),
            Err(e) => warn!(error = %e, "openviking health probe failed; continuing"),
        }
    }

    /// Commit a session's accumulated messages and return the [`CommitAck`]
    /// (including any extraction `task_id`). The production write path
    /// ([`Memory::on_session_end`]) commits fire-and-forget; callers that must
    /// know when server-side extraction actually *finished* — benchmarks,
    /// diagnostics — commit through this and then poll [`Self::wait_commit_task`].
    pub async fn commit_session(
        &self,
        user_id: &str,
        agent_id: &str,
        session_id: &str,
    ) -> Result<CommitAck> {
        let scope = VikingScope::new(user_id, agent_id);
        self.inner
            .commit(scope, session_id, self.inner.timeouts.write, None)
            .await
    }

    /// Poll a commit's extraction `task_id` to a terminal state (or `timeout`).
    /// The true completion signal `recall`-count stability can only approximate.
    pub async fn wait_commit_task(
        &self,
        user_id: &str,
        agent_id: &str,
        task_id: &str,
        interval: Duration,
        timeout: Duration,
    ) -> CommitTaskOutcome {
        self.inner
            .poll_commit_task(
                VikingScope::new(user_id, agent_id),
                task_id,
                interval,
                timeout,
                None,
                || false,
            )
            .await
    }

    /// Context-free recall for harnesses/diagnostics: `POST /search/find` with
    /// the configured `top_k`, returning the recalled memory contents. The
    /// trait [`Memory::recall`] is this plus `MemoryContext` plumbing and
    /// failure-swallowing.
    pub async fn recall_for(
        &self,
        user_id: &str,
        agent_id: &str,
        query: &str,
    ) -> Result<Vec<RecalledMemory>> {
        let scope = VikingScope::new(user_id, agent_id);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({"query": query, "top_k": self.inner.top_k});
        let resp = self
            .inner
            .post_json(
                "/api/v1/search/find",
                scope,
                &body,
                self.inner.timeouts.recall,
                None,
            )
            .await?;
        Ok(parse_search_results(&resp))
    }

    /// Context-free single-message write: `POST /sessions/{session_id}/messages`.
    /// Writes `content` **verbatim** (no truncation). Commit the session
    /// afterward — via [`Self::commit_session`] — to trigger extraction.
    pub async fn add_message(
        &self,
        user_id: &str,
        agent_id: &str,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let scope = VikingScope::new(user_id, agent_id);
        let path = format!("/api/v1/sessions/{session_id}/messages");
        let body = json!({"role": role, "content": content});
        self.inner
            .post_json(&path, scope, &body, self.inner.timeouts.write, None)
            .await?;
        Ok(())
    }
}

fn concat_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(t) = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

/// Whether `uri` points into a deletable memory tree — `viking://user/…/memories`
/// or `viking://agent/…/memories` (an optional single space segment before
/// `memories`). Mirrors the official plugin's `MEMORY_URI_PATTERNS`; the
/// load-bearing guard that stops `viking_forget` deleting resources / skills /
/// arbitrary paths.
fn is_memory_uri(uri: &str) -> bool {
    let rest = match uri
        .strip_prefix("viking://user/")
        .or_else(|| uri.strip_prefix("viking://agent/"))
    {
        Some(r) => r,
        None => return false,
    };
    let mut segs = rest.split('/');
    match (segs.next(), segs.next()) {
        (Some("memories"), _) => true,
        (Some(space), Some("memories")) if !space.is_empty() => true,
        _ => false,
    }
}

fn clamp_score(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

fn item_score(item: &Value) -> f64 {
    clamp_score(item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0))
}

fn item_uri(item: &Value) -> &str {
    item.get("uri").and_then(|v| v.as_str()).unwrap_or("")
}

/// Memories from a `find` response: `result.memories` (server wraps the hit
/// list under `result`).
fn find_memories(resp: &Value) -> Vec<Value> {
    resp.get("result")
        .and_then(|r| r.get("memories"))
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Sort by score desc, drop sub-threshold (and non-leaf when `leaf_only`),
/// dedup by URI, cap at `limit`. Mirrors the plugin's `postProcessMemories`.
fn postprocess_memories(
    mut items: Vec<Value>,
    limit: usize,
    score_threshold: f64,
    leaf_only: bool,
) -> Vec<Value> {
    items.sort_by(|a, b| {
        item_score(b)
            .partial_cmp(&item_score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if leaf_only && item.get("level").and_then(|v| v.as_i64()) != Some(2) {
            continue;
        }
        if item_score(&item) < score_threshold {
            continue;
        }
        let uri = item_uri(&item).to_string();
        if !uri.is_empty() && !seen.insert(uri) {
            continue;
        }
        out.push(item);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Total memories extracted across the 6 categories in a commit / task result.
fn count_extracted(v: &Value) -> u64 {
    v.get("memories_extracted")
        .and_then(|m| m.as_object())
        .map(|o| o.values().filter_map(|x| x.as_u64()).sum())
        .unwrap_or(0)
}

#[async_trait]
impl Memory for OpenVikingMemory {
    async fn recall(
        &self,
        ctx: &MemoryContext,
        query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>> {
        match self
            .recall_for(ctx.user_id(), ctx.agent_id().as_str(), &concat_text(query))
            .await
        {
            Ok(memories) => Ok(memories),
            Err(e) => {
                warn!(error = %e, "openviking recall failed (timeout or backend)");
                Ok(Vec::new())
            }
        }
    }

    async fn on_turn_complete(
        &self,
        ctx: &MemoryContext,
        user_input: &[ContentBlock],
        final_output: &[ContentBlock],
    ) -> Result<()> {
        let user_text = concat_text(user_input);
        let assistant_text = concat_text(final_output);
        if user_text.is_empty() && assistant_text.is_empty() {
            return Ok(());
        }
        let scope = VikingScope::new(ctx.user_id(), ctx.agent_id().as_str());
        let sid = ctx.session_id().as_str();
        if !user_text.is_empty()
            && let Err(e) = self
                .add_message(scope.user, scope.agent, sid, "user", &user_text)
                .await
        {
            debug!(error = %e, "openviking on_turn_complete user msg failed");
        }
        if !assistant_text.is_empty()
            && let Err(e) = self
                .add_message(scope.user, scope.agent, sid, "assistant", &assistant_text)
                .await
        {
            debug!(error = %e, "openviking on_turn_complete assistant msg failed");
        }
        Ok(())
    }

    async fn on_session_end(&self, ctx: &MemoryContext, transcript: &[ChatMessage]) -> Result<()> {
        if transcript.is_empty() {
            return Ok(());
        }
        // Fire-and-forget: the extraction `task_id` the commit hands back is
        // discarded here (the user never waits on extraction). Callers needing
        // true completion go through `commit_session` + `wait_commit_task`.
        if let Err(e) = self
            .commit_session(
                ctx.user_id(),
                ctx.agent_id().as_str(),
                ctx.session_id().as_str(),
            )
            .await
        {
            warn!(error = %e, "openviking session commit failed");
        }
        Ok(())
    }

    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
        let inner = &self.inner;
        vec![
            tool_pair(Arc::new(VikingRecallTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(VikingStoreTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(VikingForgetTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(VikingArchiveExpandTool {
                inner: Arc::clone(inner),
            })),
        ]
    }
}

fn parse_search_results(resp: &Value) -> Vec<RecalledMemory> {
    let Some(result) = resp.get("result") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ctx_type in ["memories", "resources"] {
        if let Some(items) = result.get(ctx_type).and_then(|v| v.as_array()) {
            for item in items {
                let uri = item.get("uri").and_then(|v| v.as_str()).unwrap_or_default();
                let abstract_text = item
                    .get("abstract")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if abstract_text.is_empty() {
                    continue;
                }
                out.push(RecalledMemory {
                    content: if uri.is_empty() {
                        abstract_text.to_string()
                    } else {
                        format!("{abstract_text} ({uri})")
                    },
                });
            }
        }
    }
    out
}

fn tool_pair(tool: Arc<dyn Tool>) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![ToolCapability::Http],
        channels: Vec::new(),
        deferred: false,
    };
    (tool, manifest)
}

/// Project a `find` hit into the compact shape the tools surface.
fn format_hit(item: &Value) -> Value {
    json!({
        "uri": item_uri(item),
        "abstract": item.get("abstract").and_then(|v| v.as_str()).unwrap_or(""),
        "score": (item_score(item) * 1000.0).round() / 1000.0,
    })
}

// ---------------------------------------------------------------------------
// Tools — the four `viking_*` tools (recall / store / forget / archive_expand),
// ported from the official OpenViking `openclaw-plugin`.
// ---------------------------------------------------------------------------

struct VikingRecallTool {
    inner: Arc<OpenVikingInner>,
}

impl VikingRecallTool {
    async fn find(
        &self,
        query: &str,
        target_uri: &str,
        request_limit: usize,
        scope: VikingScope<'_>,
        events: &Arc<dyn ToolEventSink>,
    ) -> Vec<Value> {
        let body = json!({
            "query": query,
            "target_uri": target_uri,
            "top_k": request_limit,
            "score_threshold": 0,
        });
        match self
            .inner
            .post_json(
                "/api/v1/search/find",
                scope,
                &body,
                self.inner.timeouts.http,
                Some(events),
            )
            .await
        {
            Ok(resp) => find_memories(&resp),
            Err(e) => {
                debug!(error = %e, target_uri, "openviking recall find leg failed");
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl Tool for VikingRecallTool {
    fn name(&self) -> &str {
        TOOL_RECALL
    }
    fn description(&self) -> String {
        "Search long-term memories from OpenViking. Use when you need past user preferences, \
         facts, or decisions. Without a targetUri this searches both the user and agent memory \
         roots and merges the results."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query."},
                "limit": {"type": "integer", "description": "Max results (default: configured top_k)."},
                "scoreThreshold": {"type": "number", "description": "Minimum score 0-1 (default: 0.15)."},
                "targetUri": {"type": "string", "description": "Restrict search to a single viking:// root (default: user + agent memories)."}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("query is required".into()))?;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n.max(1) as usize)
            .unwrap_or(self.inner.top_k);
        let score_threshold = params
            .get("scoreThreshold")
            .and_then(|v| v.as_f64())
            .map(clamp_score)
            .unwrap_or(DEFAULT_SCORE_THRESHOLD);
        // Over-fetch then post-filter to leaves, like the official plugin.
        let request_limit = (limit * 4).max(20);
        let scope = VikingScope::new(ctx.user.id.as_str(), ctx.agent_id.as_str());

        let merged = if let Some(target_uri) = params.get("targetUri").and_then(|v| v.as_str()) {
            self.find(query, target_uri, request_limit, scope, &ctx.events)
                .await
        } else {
            // Both roots concurrently; a failed leg contributes nothing.
            let (user_hits, agent_hits) = tokio::join!(
                self.find(query, USER_MEMORIES_URI, request_limit, scope, &ctx.events),
                self.find(query, AGENT_MEMORIES_URI, request_limit, scope, &ctx.events),
            );
            let mut all = user_hits;
            all.extend(agent_hits);
            all
        };

        let memories = postprocess_memories(merged, limit, score_threshold, true);
        if memories.is_empty() {
            return Ok(ToolOutput::Json(json!({
                "result": "No relevant OpenViking memories found.",
                "count": 0,
            })));
        }
        let results: Vec<Value> = memories.iter().map(format_hit).collect();
        Ok(ToolOutput::Json(json!({
            "results": results,
            "count": results.len(),
        })))
    }
}

struct VikingStoreTool {
    inner: Arc<OpenVikingInner>,
}

#[async_trait]
impl Tool for VikingStoreTool {
    fn name(&self) -> &str {
        TOOL_STORE
    }
    fn max_timeout(&self) -> Duration {
        self.inner.timeouts.store_max
    }
    fn description(&self) -> String {
        "Store text in the OpenViking memory pipeline: write it to a session and run server-side \
         memory extraction. Use for important information the agent should remember long-term."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Information to store as memory source text."},
                "role": {"type": "string", "description": "Session message role (default: user)."},
                "sessionId": {"type": "string", "description": "Existing OpenViking session ID (default: a fresh temporary session)."}
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("text is required".into()))?;
        if text.is_empty() {
            return Err(ToolError::InvalidParams("text cannot be empty".into()));
        }
        let role = params
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user");
        let scope = VikingScope::new(ctx.user.id.as_str(), ctx.agent_id.as_str());
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("baybo-store-{}", Uuid::new_v4().simple()));

        let msg_path = format!("/api/v1/sessions/{session_id}/messages");
        let body = json!({"role": role, "content": text});
        self.inner
            .post_json(
                &msg_path,
                scope,
                &body,
                self.inner.timeouts.http,
                Some(&ctx.events),
            )
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let ack = self
            .inner
            .commit(
                scope,
                &session_id,
                self.inner.timeouts.http,
                Some(&ctx.events),
            )
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let mut status = ack.status;
        let mut memories = ack.memories_extracted;

        // Phase 2 (extraction) runs as a background task; poll it to completion
        // so the model gets a real count, bounded by the store-poll timeout and the
        // user's cancellation.
        if let Some(task_id) = ack.task_id.as_deref()
            && status != "completed"
            && status != "failed"
        {
            let outcome = self
                .inner
                .poll_commit_task(
                    scope,
                    task_id,
                    STORE_POLL_INTERVAL,
                    self.inner.timeouts.store_poll,
                    Some(&ctx.events),
                    || ctx.cancellation_token.is_cancelled(),
                )
                .await;
            match outcome.status.as_str() {
                "completed" => {
                    status = "completed".into();
                    memories = outcome.memories_extracted;
                }
                "failed" => status = "failed".into(),
                "timeout" => status = "timeout".into(),
                // cancelled / poll error → keep the commit's status, matching
                // the original loop's early `break` without a status change.
                _ => {}
            }
        }

        Ok(ToolOutput::Json(json!({
            "action": "stored",
            "session_id": session_id,
            "status": status,
            "memories_extracted": memories,
        })))
    }
}

struct VikingForgetTool {
    inner: Arc<OpenVikingInner>,
}

impl VikingForgetTool {
    async fn delete_uri(&self, uri: &str, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        self.inner
            .delete(
                "/api/v1/fs",
                VikingScope::new(ctx.user.id.as_str(), ctx.agent_id.as_str()),
                &[("uri", uri), ("recursive", "false")],
                self.inner.timeouts.http,
                Some(&ctx.events),
            )
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolOutput::Json(json!({
            "action": "deleted",
            "uri": uri,
        })))
    }
}

#[async_trait]
impl Tool for VikingForgetTool {
    fn name(&self) -> &str {
        TOOL_FORGET
    }
    fn description(&self) -> String {
        "Forget a memory by exact viking:// URI, or pass a query to search-and-delete when a \
         single strong match is found. Only memory URIs are deletable."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string", "description": "Exact memory URI to delete."},
                "query": {"type": "string", "description": "Search query to find a memory to delete."},
                "targetUri": {"type": "string", "description": "Search scope URI (default: user memories)."},
                "limit": {"type": "integer", "description": "Search candidate count (default: 5)."},
                "scoreThreshold": {"type": "number", "description": "Minimum candidate score 0-1 (default: 0.15)."}
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if let Some(uri) = params.get("uri").and_then(|v| v.as_str()) {
            if !is_memory_uri(uri) {
                return Ok(ToolOutput::Error(format!(
                    "Refusing to delete non-memory URI: {uri}"
                )));
            }
            return self.delete_uri(uri, ctx).await;
        }

        let Some(query) = params.get("query").and_then(|v| v.as_str()) else {
            return Err(ToolError::InvalidParams("provide uri or query".into()));
        };

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n.max(1) as usize)
            .unwrap_or(FORGET_DEFAULT_LIMIT);
        let score_threshold = params
            .get("scoreThreshold")
            .and_then(|v| v.as_f64())
            .map(clamp_score)
            .unwrap_or(DEFAULT_SCORE_THRESHOLD);
        let target_uri = params
            .get("targetUri")
            .and_then(|v| v.as_str())
            .unwrap_or(USER_MEMORIES_URI);
        let request_limit = (limit * 4).max(20);

        let body = json!({
            "query": query,
            "target_uri": target_uri,
            "top_k": request_limit,
            "score_threshold": 0,
        });
        let resp = self
            .inner
            .post_json(
                "/api/v1/search/find",
                VikingScope::new(ctx.user.id.as_str(), ctx.agent_id.as_str()),
                &body,
                self.inner.timeouts.http,
                Some(&ctx.events),
            )
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let candidates: Vec<Value> =
            postprocess_memories(find_memories(&resp), request_limit, score_threshold, true)
                .into_iter()
                .filter(|item| is_memory_uri(item_uri(item)))
                .collect();

        if candidates.is_empty() {
            return Ok(ToolOutput::Json(json!({
                "action": "none",
                "result": "No matching leaf memory candidates found. Try a more specific query.",
            })));
        }

        let top = &candidates[0];
        if candidates.len() == 1 && item_score(top) >= FORGET_AUTO_DELETE_SCORE {
            let uri = item_uri(top).to_string();
            return self.delete_uri(&uri, ctx).await;
        }

        let listed: Vec<Value> = candidates.iter().take(limit).map(format_hit).collect();
        Ok(ToolOutput::Json(json!({
            "action": "candidates",
            "result": format!("Found {} candidates; pass uri to delete one.", listed.len()),
            "candidates": listed,
        })))
    }
}

struct VikingArchiveExpandTool {
    inner: Arc<OpenVikingInner>,
}

#[async_trait]
impl Tool for VikingArchiveExpandTool {
    fn name(&self) -> &str {
        TOOL_ARCHIVE_EXPAND
    }
    fn description(&self) -> String {
        "Retrieve the original messages from a compressed session archive. Use when a session \
         summary lacks specific details such as exact commands, file paths, code snippets, or \
         config values. Check the archive index for the right archive ID."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "archiveId": {"type": "string", "description": "Archive ID from the archive index (e.g. \"archive_002\")."}
            },
            "required": ["archiveId"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let archive_id = params
            .get("archiveId")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidParams("archiveId is required".into()))?;
        let sid = ctx.session_id.as_str();
        if sid.is_empty() {
            return Ok(ToolOutput::Error("no active session".into()));
        }

        let path = format!("/api/v1/sessions/{sid}/archives/{archive_id}");
        let resp = self
            .inner
            .get(
                &path,
                VikingScope::new(ctx.user.id.as_str(), ctx.agent_id.as_str()),
                &[],
                self.inner.timeouts.http,
                Some(&ctx.events),
            )
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let detail = resp.get("result").unwrap_or(&resp);

        let messages = detail
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let body: String = messages
            .iter()
            .map(|m| {
                let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
                let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                format!("{role}: {content}")
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let resolved_id = detail
            .get("archive_id")
            .and_then(|v| v.as_str())
            .unwrap_or(archive_id);
        let abstract_text = detail
            .get("abstract")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let header = format!("## {resolved_id}\n**Messages**: {}", messages.len());
        let content = if abstract_text.is_empty() {
            format!("{header}\n\n{body}")
        } else {
            format!("{header}\n**Summary**: {abstract_text}\n\n{body}")
        };

        Ok(ToolOutput::Json(json!({
            "action": "expanded",
            "archive_id": resolved_id,
            "message_count": messages.len(),
            "content": content,
        })))
    }
}

/// Parse `MemoryConfig.extra` into a typed [`OpenVikingConfig`].
pub fn parse_extra(extra: &Value) -> Result<OpenVikingConfig> {
    if extra.is_null() {
        return Ok(OpenVikingConfig::default());
    }
    serde_json::from_value(extra.clone())
        .map_err(|e| MemoryError::Internal(anyhow!("openviking config parse failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = OpenVikingConfig::default();
        assert_eq!(cfg.endpoint(), DEFAULT_ENDPOINT);
        assert_eq!(cfg.account(), DEFAULT_ACCOUNT);
        assert_eq!(cfg.top_k(), DEFAULT_TOP_K);
    }

    #[test]
    fn default_config_serializes_to_empty_object() {
        // Every field is `None` by default; `skip_serializing_if` should
        // elide each one, so the JSON written into `MemoryConfig.extra` is
        // `{}` rather than `{"endpoint": null, "api_key_name": null, ...}`.
        let json = serde_json::to_value(OpenVikingConfig::default()).unwrap();
        assert_eq!(json, json!({}));
    }

    #[test]
    fn parse_extra_handles_null_and_object() {
        assert!(parse_extra(&Value::Null).is_ok());
        let cfg = parse_extra(&json!({"endpoint": "http://example:1234"})).unwrap();
        assert_eq!(cfg.endpoint(), "http://example:1234");
    }

    #[test]
    fn parse_search_results_extracts_memories_and_resources() {
        let resp = json!({
            "result": {
                "memories": [
                    {"uri": "viking://m/1", "abstract": "fact A", "score": 0.9}
                ],
                "resources": [
                    {"uri": "viking://r/2", "abstract": "doc B", "score": 0.5}
                ]
            }
        });
        let out = parse_search_results(&resp);
        assert_eq!(out.len(), 2);
        assert!(out[0].content.contains("fact A"));
        assert!(out[0].content.contains("viking://m/1"));
    }

    #[test]
    fn parse_search_results_skips_items_without_abstract() {
        let resp = json!({
            "result": {
                "memories": [
                    {"uri": "viking://m/1", "abstract": ""},
                    {"uri": "viking://m/2"}
                ]
            }
        });
        assert!(parse_search_results(&resp).is_empty());
    }

    #[test]
    fn is_memory_uri_matches_user_and_agent_memory_trees() {
        assert!(is_memory_uri("viking://user/memories"));
        assert!(is_memory_uri("viking://user/memories/preferences/mem_x.md"));
        assert!(is_memory_uri("viking://user/space7/memories/x.md"));
        assert!(is_memory_uri("viking://agent/memories/cases/c.md"));
        // Not a memory tree.
        assert!(!is_memory_uri("viking://user/resources/doc.md"));
        assert!(!is_memory_uri("viking://agent/skills/s.md"));
        assert!(!is_memory_uri("viking://resources/x"));
        assert!(!is_memory_uri("https://example.com"));
        // `memories` must be a full path segment, not a prefix.
        assert!(!is_memory_uri("viking://user/memoriesX/y"));
    }

    #[test]
    fn postprocess_filters_sorts_dedups_and_caps() {
        let items = vec![
            json!({"uri": "a", "score": 0.5, "level": 2}),
            json!({"uri": "b", "score": 0.9, "level": 2}),
            json!({"uri": "a", "score": 0.8, "level": 2}), // dup uri
            json!({"uri": "c", "score": 0.95, "level": 1}), // non-leaf
            json!({"uri": "d", "score": 0.1, "level": 2}), // sub-threshold
        ];
        let out = postprocess_memories(items, 10, 0.3, true);
        let uris: Vec<&str> = out.iter().map(item_uri).collect();
        // b (0.9) then a (0.8, first kept beats the 0.5 dup); c filtered (non-leaf),
        // d filtered (sub-threshold).
        assert_eq!(uris, vec!["b", "a"]);

        let capped = postprocess_memories(
            vec![
                json!({"uri": "x", "score": 0.9, "level": 2}),
                json!({"uri": "y", "score": 0.8, "level": 2}),
            ],
            1,
            0.0,
            true,
        );
        assert_eq!(capped.len(), 1);
        assert_eq!(item_uri(&capped[0]), "x");
    }

    #[test]
    fn count_extracted_sums_categories() {
        let commit = json!({"memories_extracted": {"preferences": 2, "events": 3, "cases": 0}});
        assert_eq!(count_extracted(&commit), 5);
        assert_eq!(count_extracted(&json!({})), 0);
    }
}
