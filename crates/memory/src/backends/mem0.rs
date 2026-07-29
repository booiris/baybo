//! Mem0 memory backend — the managed Platform API by default, or a self-hosted
//! OSS server when `self_hosted` is set (see "Self-hosted (OSS) mode" below).
//!
//! Hosted SaaS memory with server-side LLM fact extraction, semantic search,
//! and reranking via the Mem0 REST API. Two coexisting surfaces:
//!
//! **Automatic hooks** (the core's recall / write path):
//! - **`recall`**: `POST /v2/memories/search/` with `{filters, rerank, top_k}`.
//! - **`on_turn_complete`**: `POST /v1/memories/` with `{messages, user_id, agent_id}`.
//! - **`on_session_end`**: no-op (Mem0 has no session concept; extraction is
//!   immediate on `add`).
//!
//! **Tools** (the model's explicit-signal path) — the eight-tool surface ported
//! from the Mem0 `openclaw` plugin, each `mem0_`-prefixed to namespace this
//! backend and mapped onto a Mem0 REST endpoint:
//!
//! | Tool | Method + path |
//! | --- | --- |
//! | `mem0_search` | `POST /v2/memories/search/` |
//! | `mem0_add` | `POST /v1/memories/` (`infer: false` — stored verbatim) |
//! | `mem0_get` | `GET /v1/memories/{id}/` |
//! | `mem0_list` | `POST /v2/memories/` (`?page&page_size`) |
//! | `mem0_update` | `PUT /v1/memories/{id}/` |
//! | `mem0_delete` | `DELETE /v1/memories/{id}/` or `DELETE /v1/memories/?user_id=` |
//! | `mem0_event_list` | `GET /v1/events/` |
//! | `mem0_event_status` | `GET /v1/event/{id}/` |
//!
//! Per-user scoping is fixed to the caller's `user_id` (the session user) at
//! every call — never overridable by a tool param, so one user's tools cannot
//! read, write, or delete another user's memories. An optional `scope:
//! "session"` narrows reads to the current session via Mem0's `run_id` (sourced
//! from `ToolContext::session_id`). `agent_id` defaults to `"baybo"` (deployment
//! identity) on writes and is the one identity a tool call may still set.
//!
//! Failures are routed through a 5-failure / 120 s circuit breaker that pauses
//! API calls after sustained outages.
//!
//! ## Self-hosted (OSS) mode
//!
//! Set `self_hosted: true` (with `base_url` pointing at your server) to target
//! the open-source mem0 server instead of the managed Platform. Its API
//! differs: unversioned paths (`/memories`, `/search`), no `Token` auth, and
//! synchronous extraction — there is no `/events` feed, so settle is immediate
//! and the event tools say as much. Platform paths are folded automatically
//! ([`Mem0Inner::map_path`]); search scopes by `user_id` rather than the v2
//! filter shape.
//!
//! # References
//!
//! - Mem0 project: <https://github.com/mem0ai/mem0>
//! - Mem0 Platform API docs: <https://docs.mem0.ai/api-reference>
//! - Tool surface + REST endpoint map ported from the Mem0 `openclaw` plugin's
//!   `PlatformBackend` (`openclaw/backend/platform.ts`) and its per-tool
//!   definitions (`openclaw/tools/*.ts`): `memory_search` / `add` / `get` /
//!   `list` / `update` / `delete` / `event_list` / `event_status`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use async_trait::async_trait;
use baybo_model::{ContentBlock, TrustLevel};
use baybo_security::http::ProxySettings;
use baybo_security::{SecretVault, USER_SECRET_PREFIX};
use baybo_tools::{
    Tool, ToolCapability, ToolContext, ToolError, ToolEventSink, ToolManifest, ToolOutput,
};
use baybo_trace::ToolEventPayload;
use parking_lot::Mutex;
use reqwest::{Method, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::{Memory, MemoryContext, MemoryError, RecalledMemory, Result};

const DEFAULT_BASE_URL: &str = "https://api.mem0.ai";
const DEFAULT_AGENT_ID: &str = "baybo";
const DEFAULT_TOP_K: usize = 5;
/// Default minimum similarity for `mem0_search` / search-and-delete. Mirrors
/// the openclaw plugin's `searchThreshold` default.
const DEFAULT_SEARCH_THRESHOLD: f64 = 0.3;
/// Default per-request budget for tool calls (model is already waiting).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Probe is best-effort and runs inline during `build_managers`; keep the
/// per-request budget short so an unreachable Mem0 endpoint does not stall
/// boot up to `HTTP_TIMEOUT` (mirrors openviking's `HEALTH_TIMEOUT`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Recall is on the **critical path** (turn start + each interjection), so
/// the agent loop waits inline for it. Cap the request well below
/// `HTTP_TIMEOUT` so a slow / down Mem0 endpoint degrades the turn to "no
/// recalled context" instead of stalling the user up to 30 s before the
/// first model token.
const RECALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Background-write budget for `on_turn_complete`. Detached on the runtime
/// root, so the user never waits; Mem0's server-side fact-extraction is
/// LLM-backed and can be slow under load, so give it a generous ceiling.
const WRITE_TIMEOUT: Duration = Duration::from_secs(600);
/// Default user-secret name (managed via `baybo secret add`) holding the
/// Mem0 API key. Doubles as the process-env var name when the secret
/// vault is empty.
const DEFAULT_API_KEY_NAME: &str = "MEM0_API_KEY";

/// Tool names. Exposed as constants so the runtime / tests can reference them
/// without literal-typo risk. The set mirrors the Mem0 `openclaw` plugin's
/// eight-tool surface, `mem0_`-prefixed to namespace this backend.
pub const TOOL_SEARCH: &str = "mem0_search";
pub const TOOL_ADD: &str = "mem0_add";
pub const TOOL_GET: &str = "mem0_get";
pub const TOOL_LIST: &str = "mem0_list";
pub const TOOL_UPDATE: &str = "mem0_update";
pub const TOOL_DELETE: &str = "mem0_delete";
pub const TOOL_EVENT_LIST: &str = "mem0_event_list";
pub const TOOL_EVENT_STATUS: &str = "mem0_event_status";

const BREAKER_THRESHOLD: u32 = 5;
const BREAKER_COOLDOWN: Duration = Duration::from_secs(120);

/// Page size for `mem0_list` (and the bulk fetch behind it). Mem0's list
/// endpoint paginates; one page of 100 is plenty for an agent-facing dump.
const MAX_LIST_ENTRIES: usize = 100;
/// Search depth for `mem0_delete`'s search-and-delete path before it falls
/// back to returning candidate ids for the model to disambiguate.
const DELETE_SEARCH_TOP_K: usize = 5;
/// A search-and-delete match this confident is deleted outright even when it
/// is not the only hit (mirrors the openclaw plugin's 0.9 gate).
const DELETE_AUTO_SCORE: f64 = 0.9;

/// Per-backend config deserialized from `MemoryConfig.extra`. Unset fields
/// elide from JSON via `skip_serializing_if`, so a freshly-written `extra`
/// is `{}` instead of `{"api_key_name": null, "base_url": null, ...}`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Mem0Config {
    /// Name of the user secret holding the Mem0 API key. Resolution at
    /// startup: vault entry `user_env.<api_key_name>` (managed via
    /// `baybo secret add <name>`), then process-env `<api_key_name>`.
    /// `None` defaults the name to `"MEM0_API_KEY"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_name: Option<String>,
    /// Override the Mem0 REST base URL. `None` → `https://api.mem0.ai`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Enable Mem0 server-side reranking for `recall` (more accurate, slower).
    /// `None` → `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank: Option<bool>,
    /// Max results returned by `recall`. `None` → 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    /// Target a self-hosted mem0 OSS server (`/memories`, `/search`, no auth,
    /// synchronous extraction) instead of the managed Platform API. `None` →
    /// `false` (Platform). Set `base_url` to the server when enabling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_hosted: Option<bool>,
}

impl Mem0Config {
    fn base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)
    }

    fn rerank(&self) -> bool {
        self.rerank.unwrap_or(true)
    }

    fn top_k(&self) -> usize {
        self.top_k.unwrap_or(DEFAULT_TOP_K)
    }

    pub(crate) fn self_hosted(&self) -> bool {
        self.self_hosted.unwrap_or(false)
    }
}

/// Resolve the Mem0 API key. Order:
///   1. User secret vault entry `user_env.<name>` (managed via
///      `baybo secret add <name>`).
///   2. Process env var of the same name.
///
/// `<name>` is `cfg.api_key_name` or [`DEFAULT_API_KEY_NAME`] when unset.
/// The dedicated `memory.mem0.api_key` vault path has been retired —
/// secrets now live alongside other user-managed credentials under the
/// `user_env.` namespace, manageable via the existing `baybo secret`
/// command.
pub async fn resolve_api_key(cfg: &Mem0Config, vault: Option<&SecretVault>) -> Option<String> {
    let name = cfg.api_key_name.as_deref().unwrap_or(DEFAULT_API_KEY_NAME);
    if let Some(vault) = vault {
        let key = format!("{USER_SECRET_PREFIX}{name}");
        if let Ok(Some(secret)) = vault.get_secret(&key).await
            && let Ok(s) = secret.as_str()
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

#[derive(Default)]
struct BreakerState {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

impl BreakerState {
    fn is_open(&mut self) -> bool {
        if self.consecutive_failures < BREAKER_THRESHOLD {
            return false;
        }
        match self.open_until {
            Some(deadline) if Instant::now() < deadline => true,
            _ => {
                self.consecutive_failures = 0;
                self.open_until = None;
                false
            }
        }
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.open_until = None;
    }

    fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= BREAKER_THRESHOLD {
            self.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
            warn!(
                failures = self.consecutive_failures,
                cooldown_secs = BREAKER_COOLDOWN.as_secs(),
                "mem0 circuit breaker tripped — pausing API calls"
            );
        }
    }
}

struct Mem0Inner {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    /// `true` → self-hosted OSS server (OSS paths, no auth header, no events).
    self_hosted: bool,
    rerank: bool,
    top_k: usize,
    breaker: Mutex<BreakerState>,
}

impl Mem0Inner {
    fn breaker_open(&self) -> bool {
        self.breaker.lock().is_open()
    }

    fn record_success(&self) {
        self.breaker.lock().record_success();
    }

    fn record_failure(&self) {
        self.breaker.lock().record_failure();
    }

    fn url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}{}", self.map_path(path))
    }

    /// Map a Platform-API path to its self-hosted OSS-server equivalent: drop
    /// the `/v1`·`/v2` version prefix and the trailing slash, and fold
    /// `/memories/search` onto `/search`. A no-op in Platform mode.
    fn map_path<'a>(&self, path: &'a str) -> &'a str {
        if !self.self_hosted {
            return path;
        }
        let p = path.trim_end_matches('/');
        let p = p
            .strip_prefix("/v1")
            .or_else(|| p.strip_prefix("/v2"))
            .unwrap_or(p);
        if p == "/memories/search" {
            "/search"
        } else {
            p
        }
    }

    fn auth_header(&self) -> String {
        format!("Token {}", self.api_key)
    }

    /// Run one HTTP request + emit observability (same shape for every verb):
    /// - `tracing::debug!` with the request preview before the call.
    /// - `tracing::info!` with elapsed / status / response preview after.
    /// - When `events` is `Some` (the tool-call path), a `HttpFetch`
    ///   trace event lands on the surrounding `ToolCall` span so it
    ///   shows up in the trace UI alongside the tool's own activity.
    ///
    /// `body` is the JSON payload (`None` for GET / DELETE); `params` are query
    /// pairs (Mem0's bulk delete keys the scope off the query string). A 2xx
    /// with an empty body (e.g. `204 No Content` from DELETE) decodes to
    /// [`Value::Null`] rather than erroring.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        params: &[(&str, String)],
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<Value> {
        let started = Instant::now();
        let request_preview = body.map(preview_value).unwrap_or_default();
        debug!(
            backend = "mem0",
            method = %method,
            path,
            request = %request_preview,
            "mem0 outbound request"
        );

        let mut url = match reqwest::Url::parse(&self.url(path)) {
            Ok(u) => u,
            Err(e) => return Err(MemoryError::Backend(format!("mem0 bad url {path}: {e}"))),
        };
        if !params.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in params {
                pairs.append_pair(k, v);
            }
        }
        let mut req = self.client.request(method.clone(), url).timeout(timeout);
        if !self.self_hosted {
            req = req.header(header::AUTHORIZATION, self.auth_header());
        }
        if let Some(b) = body {
            req = req.json(b);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                info!(
                    backend = "mem0",
                    method = %method,
                    path,
                    elapsed_ms,
                    error = %e,
                    "mem0 HTTP send failed"
                );
                return Err(MemoryError::Backend(format!("mem0 request failed: {e}")));
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
            backend = "mem0",
            method = %method,
            path,
            status = %status,
            elapsed_ms,
            bytes,
            response = %truncate(&body_text, HTTP_PREVIEW_BYTES),
            "mem0 HTTP roundtrip"
        );
        if let Some(sink) = events {
            sink.emit(
                "mem0_http",
                ToolEventPayload::HttpFetch {
                    status: status.as_u16(),
                    bytes,
                    content_type,
                    body_preview: Some(truncate(&body_text, HTTP_PREVIEW_BYTES)),
                },
            );
        }

        if !status.is_success() {
            return Err(MemoryError::Backend(format!(
                "mem0 {path} returned {status}: {}",
                truncate(&body_text, HTTP_PREVIEW_BYTES)
            )));
        }
        if body_text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str::<Value>(&body_text)
            .map_err(|e| MemoryError::Backend(format!("mem0 response parse failed: {e}")))
    }

    /// POST a JSON body. Thin wrapper over [`Self::request`] for the
    /// recall / write / probe paths that only ever POST.
    async fn post_json(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
        events: Option<&Arc<dyn ToolEventSink>>,
    ) -> Result<Value> {
        self.request(Method::POST, path, Some(body), &[], timeout, events)
            .await
    }
}

/// Body preview cap shared by both the tracing logs and the
/// `HttpFetch` trace event payload. Wide enough to give a useful
/// snippet for debugging, short enough to keep logs / traces compact.
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

/// Mem0 memory backend. Construct via [`Mem0Memory::new`] inside
/// `runtime.rs::build_managers`.
pub struct Mem0Memory {
    inner: Arc<Mem0Inner>,
}

impl Mem0Memory {
    /// Build the backend from typed config + resolved API key. The
    /// `extra` blob from [`baybo_config::MemoryConfig`] is parsed into a
    /// [`Mem0Config`] by the caller (see [`Mem0Config`] / `serde_json::from_value`).
    /// `proxy` threads the deployment-configured egress proxy through
    /// [`baybo_security::http::client_builder`] — the crate-wide outbound
    /// chokepoint.
    pub fn new(cfg: Mem0Config, api_key: String, proxy: Option<&ProxySettings>) -> Result<Self> {
        let self_hosted = cfg.self_hosted();
        // A self-hosted OSS server needs no key; the Platform API does.
        if api_key.is_empty() && !self_hosted {
            return Err(MemoryError::Backend(
                "mem0 API key missing — run `baybo secret add MEM0_API_KEY` \
                 (or set the MEM0_API_KEY env var)"
                    .into(),
            ));
        }
        let client = baybo_security::http::client_builder(proxy)
            .map_err(|e| MemoryError::Backend(format!("mem0 client build failed: {e}")))?
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| MemoryError::Backend(format!("mem0 client build failed: {e}")))?;
        let inner = Arc::new(Mem0Inner {
            client,
            base_url: cfg.base_url().to_string(),
            api_key,
            self_hosted,
            rerank: cfg.rerank(),
            top_k: cfg.top_k(),
            breaker: Mutex::new(BreakerState::default()),
        });
        Ok(Self { inner })
    }

    /// Best-effort startup probe: a small `get_all` call with a tight
    /// [`PROBE_TIMEOUT`] so an unreachable Mem0 endpoint does not stall
    /// `build_managers`. Logs `warn` on failure but does not block startup
    /// — the breaker handles persistent outages later.
    pub async fn probe(&self) {
        let body = json!({
            "filters": {"AND": [{"user_id": "__baybo_probe__"}]},
            "page": 1,
            "page_size": 1,
        });
        if let Err(e) = self
            .inner
            .post_json("/v2/memories/", &body, PROBE_TIMEOUT, None)
            .await
        {
            warn!(error = %e, "mem0 startup probe failed; continuing");
        }
    }

    /// Context-free recall for harnesses/diagnostics: `POST /v2/memories/search/`
    /// scoped to `user_id`. The trait [`Memory::recall`] is this plus the
    /// circuit breaker and failure-swallowing.
    pub async fn recall_for(&self, user_id: &str, query: &str) -> Result<Vec<RecalledMemory>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let body = if self.inner.self_hosted {
            json!({"query": query, "user_id": user_id, "limit": self.inner.top_k})
        } else {
            json!({
                "query": query,
                "filters": read_filters(user_id),
                "rerank": self.inner.rerank,
                "top_k": self.inner.top_k,
            })
        };
        let resp = self
            .inner
            .post_json("/v2/memories/search/", &body, RECALL_TIMEOUT, None)
            .await?;
        Ok(parse_search_results(&resp))
    }

    /// Context-free turn write for harnesses: `POST /v1/memories/` with one
    /// user+assistant pair under `user_id`. Extraction runs async server-side;
    /// poll its completion with [`Self::wait_events_completed`].
    pub async fn add_turn(
        &self,
        user_id: &str,
        user_text: &str,
        assistant_text: &str,
    ) -> Result<()> {
        let body = json!({
            "messages": [
                {"role": "user", "content": user_text},
                {"role": "assistant", "content": assistant_text},
            ],
            "user_id": user_id,
            "agent_id": DEFAULT_AGENT_ID,
        });
        self.inner
            .post_json("/v1/memories/", &body, WRITE_TIMEOUT, None)
            .await?;
        Ok(())
    }

    /// Poll the account-global `GET /v1/events/` feed until every event is
    /// `completed` (true extraction completion) or `timeout` elapses. Assumes a
    /// dedicated project — see the bench README's isolation note. Mirrors the
    /// `mem0_event_list` tool's read.
    pub async fn wait_events_completed(&self, interval: Duration, timeout: Duration) -> bool {
        // The OSS server extracts synchronously inside `add` — there is no
        // event feed and nothing to wait for, so settle is immediate.
        if self.inner.self_hosted {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(resp) = self
                .inner
                .request(Method::GET, "/v1/events/", None, &[], HTTP_TIMEOUT, None)
                .await
            {
                let items = result_items(&resp);
                if !items.is_empty()
                    && items
                        .iter()
                        .all(|e| e.get("status").and_then(|s| s.as_str()) == Some("completed"))
                {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(interval).await;
        }
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

/// Build a Mem0 v2 filter object for the tool paths. Always wraps the
/// conditions in `AND` (Mem0 accepts a single-element `AND`, and recall
/// already relies on that shape). A caller-supplied `extra` that itself
/// contains `AND` / `OR` is used verbatim; otherwise its top-level keys are
/// folded in as additional conditions (mirrors the openclaw plugin's
/// `_buildFilters`).
fn build_filters(
    user_id: &str,
    agent_id: Option<&str>,
    run_id: Option<&str>,
    categories: Option<&[String]>,
    extra: Option<&Value>,
) -> Value {
    if let Some(extra) = extra
        && (extra.get("AND").is_some() || extra.get("OR").is_some())
    {
        return extra.clone();
    }
    let mut conds: Vec<Value> = vec![json!({"user_id": user_id})];
    if let Some(agent_id) = agent_id {
        conds.push(json!({"agent_id": agent_id}));
    }
    if let Some(run_id) = run_id {
        conds.push(json!({"run_id": run_id}));
    }
    if let Some(cats) = categories.filter(|c| !c.is_empty()) {
        conds.push(json!({"categories": {"in": cats}}));
    }
    if let Some(obj) = extra.and_then(|e| e.as_object()) {
        for (k, v) in obj {
            conds.push(json!({ k: v }));
        }
    }
    json!({ "AND": conds })
}

/// The user-only filter the recall hot path uses (`{AND: [{user_id}]}`).
fn read_filters(user_id: &str) -> Value {
    build_filters(user_id, None, None, None, None)
}

/// Resolve the optional `scope` param into a run-id filter. `"session"`
/// narrows to the current session (Mem0 `run_id`); `"long-term"` / `"all"` /
/// absent leave it unscoped — Mem0 returns the user's full set, which already
/// includes session-scoped memories.
fn scope_run_id<'a>(scope: Option<&str>, session_id: &'a str) -> Option<&'a str> {
    match scope {
        Some("session") => Some(session_id),
        _ => None,
    }
}

/// Flatten a Mem0 list/search response into its items, accepting a bare
/// array or a `{results: [...]}` / `{memories: [...]}` wrapper.
fn result_items(resp: &Value) -> Vec<Value> {
    if let Some(arr) = resp.as_array() {
        return arr.clone();
    }
    for key in ["results", "memories"] {
        if let Some(arr) = resp.get(key).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    Vec::new()
}

/// The standard tool-output when the breaker is open: surfaced to the model
/// as a recoverable error rather than a hard failure.
fn breaker_unavailable() -> ToolOutput {
    ToolOutput::Error(
        "Mem0 API temporarily unavailable (circuit breaker tripped). Will retry automatically."
            .into(),
    )
}

#[async_trait]
impl Memory for Mem0Memory {
    async fn recall(
        &self,
        ctx: &MemoryContext,
        query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>> {
        if self.inner.breaker_open() {
            return Ok(Vec::new());
        }
        match self.recall_for(ctx.user_id(), &concat_text(query)).await {
            Ok(memories) => {
                self.inner.record_success();
                Ok(memories)
            }
            Err(e) => {
                self.inner.record_failure();
                warn!(error = %e, "mem0 recall failed (timeout or backend)");
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
        if self.inner.breaker_open() {
            return Ok(());
        }
        let user_text = concat_text(user_input);
        let assistant_text = concat_text(final_output);
        if user_text.is_empty() && assistant_text.is_empty() {
            return Ok(());
        }
        match self
            .add_turn(ctx.user_id(), &user_text, &assistant_text)
            .await
        {
            Ok(()) => {
                self.inner.record_success();
                Ok(())
            }
            Err(e) => {
                self.inner.record_failure();
                warn!(error = %e, "mem0 on_turn_complete failed");
                Ok(())
            }
        }
    }

    async fn on_session_end(
        &self,
        _ctx: &MemoryContext,
        _transcript: &[baybo_model::ChatMessage],
    ) -> Result<()> {
        Ok(())
    }

    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
        let inner = &self.inner;
        vec![
            tool_pair(Arc::new(Mem0SearchTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(Mem0AddTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(Mem0GetTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(Mem0ListTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(Mem0UpdateTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(Mem0DeleteTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(Mem0EventListTool {
                inner: Arc::clone(inner),
            })),
            tool_pair(Arc::new(Mem0EventStatusTool {
                inner: Arc::clone(inner),
            })),
        ]
    }
}

fn parse_search_results(resp: &Value) -> Vec<RecalledMemory> {
    result_items(resp)
        .iter()
        .filter_map(|item| {
            let text = item.get("memory").and_then(|v| v.as_str())?;
            if text.is_empty() {
                None
            } else {
                Some(RecalledMemory {
                    content: text.to_string(),
                })
            }
        })
        .collect()
}

fn tool_pair(tool: Arc<dyn Tool>) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![ToolCapability::Http],
        channels: Vec::new(),
    };
    (tool, manifest)
}

// ---------------------------------------------------------------------------
// Tools — the eight-tool `mem0_*` surface (search / add / get / list /
// update / delete / event_list / event_status), each mapped onto a Mem0
// REST endpoint. Ported from the Mem0 `openclaw` plugin's `tools/*.ts`.
// ---------------------------------------------------------------------------

struct Mem0SearchTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0SearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH
    }

    fn description(&self) -> String {
        "Search long-term memories by meaning. Returns relevant facts ranked by similarity. \
         Optionally scope to the current session, filter by category, or pass an advanced filter."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for."},
                "limit": {"type": "integer", "description": "Max results (default: configured top_k, max 50)."},
                "scope": {"type": "string", "enum": ["session", "long-term", "all"], "description": "\"all\" (default), \"session\" (current session only), or \"long-term\"."},
                "categories": {"type": "array", "items": {"type": "string"}, "description": "Filter by category."},
                "filters": {"type": "object", "description": "Advanced Mem0 filter object (AND/OR/operators)."},
                "agentId": {"type": "string", "description": "Filter to a specific agent namespace."}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("query is required".into()))?;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(50) as usize)
            .unwrap_or(self.inner.top_k);
        let user_id = ctx.user.id.as_str();
        let agent_id = params.get("agentId").and_then(|v| v.as_str());
        let run_id = scope_run_id(
            params.get("scope").and_then(|v| v.as_str()),
            ctx.session_id.as_str(),
        );
        let categories: Option<Vec<String>> = params
            .get("categories")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            });
        // OSS scopes by user_id and ignores the Platform v2 filter shape
        // (agent/run/category/advanced filters), so they degrade to user scope.
        let body = if self.inner.self_hosted {
            json!({"query": query, "user_id": user_id, "limit": limit})
        } else {
            json!({
                "query": query,
                "top_k": limit,
                "threshold": DEFAULT_SEARCH_THRESHOLD,
                "rerank": self.inner.rerank,
                "filters": build_filters(user_id, agent_id, run_id, categories.as_deref(), params.get("filters")),
            })
        };
        match self
            .inner
            .request(
                Method::POST,
                "/v2/memories/search/",
                Some(&body),
                &[],
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(resp) => {
                self.inner.record_success();
                let items = result_items(&resp);
                if items.is_empty() {
                    return Ok(ToolOutput::Json(
                        json!({"result": "No relevant memories found."}),
                    ));
                }
                let formatted: Vec<Value> = items
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                            "memory": m.get("memory").and_then(|v| v.as_str()).unwrap_or_default(),
                            "score": m.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        })
                    })
                    .collect();
                Ok(ToolOutput::Json(json!({
                    "results": formatted,
                    "count": formatted.len(),
                })))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("{TOOL_SEARCH} failed: {e}")))
            }
        }
    }
}

struct Mem0AddTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0AddTool {
    fn name(&self) -> &str {
        TOOL_ADD
    }

    fn description(&self) -> String {
        "Store durable fact(s) about the user in long-term memory, verbatim (no server-side \
         re-extraction). Pass `text` for one fact or `facts` for several sharing one `category`. \
         Set longTerm=false to scope a fact to the current session."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "A single fact to remember."},
                "facts": {"type": "array", "items": {"type": "string"}, "description": "Several facts to store; all share one category."},
                "category": {"type": "string", "description": "e.g. identity, preference, decision, rule, project, configuration, technical, relationship."},
                "importance": {"type": "number", "description": "Importance 0.0–1.0 (stored as metadata)."},
                "metadata": {"type": "object", "description": "Additional metadata to attach."},
                "longTerm": {"type": "boolean", "description": "Long-term (default true). false → session-scoped."},
                "agentId": {"type": "string", "description": "Agent namespace (default: baybo)."}
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        let facts: Vec<String> = match params.get("facts").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .collect(),
            None => params
                .get("text")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default(),
        };
        if facts.is_empty() {
            return Err(ToolError::InvalidParams(
                "provide `text` or a non-empty `facts` array".into(),
            ));
        }
        let user_id = ctx.user.id.as_str();
        let agent_id = params
            .get("agentId")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_AGENT_ID);
        let long_term = params
            .get("longTerm")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let messages: Vec<Value> = facts
            .iter()
            .map(|f| json!({"role": "user", "content": f}))
            .collect();
        let mut body = json!({
            "messages": messages,
            "user_id": user_id,
            "agent_id": agent_id,
            "infer": false,
        });
        if !long_term {
            body["run_id"] = json!(ctx.session_id.as_str());
        }
        if let Some(cat) = params.get("category").and_then(|v| v.as_str()) {
            body["categories"] = json!([cat]);
        }
        let mut metadata = params
            .get("metadata")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(importance) = params.get("importance").and_then(|v| v.as_f64()) {
            metadata.insert("importance".into(), json!(importance));
        }
        if !metadata.is_empty() {
            body["metadata"] = Value::Object(metadata);
        }
        match self
            .inner
            .request(
                Method::POST,
                "/v1/memories/",
                Some(&body),
                &[],
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(_) => {
                self.inner.record_success();
                Ok(ToolOutput::Json(json!({
                    "result": format!("Stored {} fact(s).", facts.len()),
                    "count": facts.len(),
                })))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("{TOOL_ADD} failed: {e}")))
            }
        }
    }
}

struct Mem0GetTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0GetTool {
    fn name(&self) -> &str {
        TOOL_GET
    }

    fn description(&self) -> String {
        "Retrieve a specific memory by its ID.".into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "memoryId": {"type": "string", "description": "The memory ID to retrieve."}
            },
            "required": ["memoryId"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        let memory_id = params
            .get("memoryId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("memoryId is required".into()))?;
        let path = format!("/v1/memories/{memory_id}/");
        match self
            .inner
            .request(
                Method::GET,
                &path,
                None,
                &[],
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(resp) => {
                self.inner.record_success();
                Ok(ToolOutput::Json(json!({
                    "id": resp.get("id").and_then(|v| v.as_str()).unwrap_or(memory_id),
                    "memory": resp.get("memory").and_then(|v| v.as_str()).unwrap_or_default(),
                    "created_at": resp.get("created_at").cloned().unwrap_or(Value::Null),
                    "updated_at": resp.get("updated_at").cloned().unwrap_or(Value::Null),
                })))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("{TOOL_GET} failed: {e}")))
            }
        }
    }
}

struct Mem0ListTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0ListTool {
    fn name(&self) -> &str {
        TOOL_LIST
    }

    fn description(&self) -> String {
        "List stored memories for the user. Optional `scope` narrows to the current session.".into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {"type": "string", "enum": ["session", "long-term", "all"], "description": "\"all\" (default), \"session\", or \"long-term\"."},
                "agentId": {"type": "string", "description": "Filter to a specific agent namespace."}
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        let user_id = ctx.user.id.as_str();
        let agent_id = params.get("agentId").and_then(|v| v.as_str());
        let run_id = scope_run_id(
            params.get("scope").and_then(|v| v.as_str()),
            ctx.session_id.as_str(),
        );
        // OSS lists via GET /memories?user_id=; Platform POSTs a v2 filter + page.
        let (method, body, qp): (Method, Option<Value>, Vec<(&str, String)>) =
            if self.inner.self_hosted {
                (Method::GET, None, vec![("user_id", user_id.to_string())])
            } else {
                (
                    Method::POST,
                    Some(json!({"filters": build_filters(user_id, agent_id, run_id, None, None)})),
                    vec![
                        ("page", "1".to_string()),
                        ("page_size", MAX_LIST_ENTRIES.to_string()),
                    ],
                )
            };
        match self
            .inner
            .request(
                method,
                "/v2/memories/",
                body.as_ref(),
                &qp,
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(resp) => {
                self.inner.record_success();
                let items = result_items(&resp);
                if items.is_empty() {
                    return Ok(ToolOutput::Json(
                        json!({"result": "No memories stored yet.", "count": 0}),
                    ));
                }
                let lines: Vec<String> = items
                    .iter()
                    .map(|m| {
                        format!(
                            "{} (id: {})",
                            m.get("memory").and_then(|v| v.as_str()).unwrap_or_default(),
                            m.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                        )
                    })
                    .collect();
                let truncated = lines.len() == MAX_LIST_ENTRIES;
                Ok(ToolOutput::Json(json!({
                    "result": lines.join("\n"),
                    "count": lines.len(),
                    "truncated": truncated,
                })))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("{TOOL_LIST} failed: {e}")))
            }
        }
    }
}

struct Mem0UpdateTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0UpdateTool {
    fn name(&self) -> &str {
        TOOL_UPDATE
    }

    fn description(&self) -> String {
        "Update an existing memory's text in place. Atomic and preserves history.".into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "memoryId": {"type": "string", "description": "The memory ID to update."},
                "text": {"type": "string", "description": "The new text (replaces old)."}
            },
            "required": ["memoryId", "text"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        let memory_id = params
            .get("memoryId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("memoryId is required".into()))?;
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("text is required".into()))?;
        if text.is_empty() {
            return Err(ToolError::InvalidParams("text cannot be empty".into()));
        }
        let path = format!("/v1/memories/{memory_id}/");
        let body = json!({"text": text});
        match self
            .inner
            .request(
                Method::PUT,
                &path,
                Some(&body),
                &[],
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(_) => {
                self.inner.record_success();
                Ok(ToolOutput::Json(json!({
                    "result": format!("Updated memory {memory_id}."),
                })))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("{TOOL_UPDATE} failed: {e}")))
            }
        }
    }
}

struct Mem0DeleteTool {
    inner: Arc<Mem0Inner>,
}

impl Mem0DeleteTool {
    async fn delete_by_id(&self, memory_id: &str, ctx: &ToolContext) -> ToolOutput {
        let path = format!("/v1/memories/{memory_id}/");
        match self
            .inner
            .request(
                Method::DELETE,
                &path,
                None,
                &[],
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(_) => {
                self.inner.record_success();
                ToolOutput::Json(json!({"result": format!("Memory {memory_id} deleted.")}))
            }
            Err(e) => {
                self.inner.record_failure();
                ToolOutput::Error(format!("{TOOL_DELETE} failed: {e}"))
            }
        }
    }
}

#[async_trait]
impl Tool for Mem0DeleteTool {
    fn name(&self) -> &str {
        TOOL_DELETE
    }

    fn description(&self) -> String {
        "Delete memories. Provide `memoryId`, a `query` to search-and-delete, or `all: true` \
         (requires `confirm: true`) to wipe the user's memories."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "memoryId": {"type": "string", "description": "Specific memory ID to delete."},
                "query": {"type": "string", "description": "Search query to find and delete a memory."},
                "all": {"type": "boolean", "description": "Delete ALL of the user's memories. Requires confirm: true."},
                "confirm": {"type": "boolean", "description": "Safety gate for bulk deletion."},
                "agentId": {"type": "string", "description": "Agent namespace scope."}
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        let user_id = ctx.user.id.as_str();
        let agent_id = params.get("agentId").and_then(|v| v.as_str());

        if let Some(memory_id) = params.get("memoryId").and_then(|v| v.as_str()) {
            return Ok(self.delete_by_id(memory_id, ctx).await);
        }

        if let Some(query) = params.get("query").and_then(|v| v.as_str()) {
            let body = if self.inner.self_hosted {
                json!({"query": query, "user_id": user_id, "limit": DELETE_SEARCH_TOP_K})
            } else {
                json!({
                    "query": query,
                    "top_k": DELETE_SEARCH_TOP_K,
                    "threshold": DEFAULT_SEARCH_THRESHOLD,
                    "filters": build_filters(user_id, agent_id, None, None, None),
                })
            };
            let resp = match self
                .inner
                .request(
                    Method::POST,
                    "/v2/memories/search/",
                    Some(&body),
                    &[],
                    HTTP_TIMEOUT,
                    Some(&ctx.events),
                )
                .await
            {
                Ok(r) => {
                    self.inner.record_success();
                    r
                }
                Err(e) => {
                    self.inner.record_failure();
                    return Ok(ToolOutput::Error(format!("{TOOL_DELETE} failed: {e}")));
                }
            };
            let items = result_items(&resp);
            if items.is_empty() {
                return Ok(ToolOutput::Json(
                    json!({"result": "No matching memories found."}),
                ));
            }
            let top = &items[0];
            let top_score = top.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if items.len() == 1 || top_score > DELETE_AUTO_SCORE {
                let id = top.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                if id.is_empty() {
                    return Ok(ToolOutput::Error("matched memory has no id".into()));
                }
                return Ok(self.delete_by_id(id, ctx).await);
            }
            let candidates: Vec<Value> = items
                .iter()
                .map(|m| {
                    json!({
                        "id": m.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                        "memory": m.get("memory").and_then(|v| v.as_str()).unwrap_or_default(),
                        "score": m.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    })
                })
                .collect();
            return Ok(ToolOutput::Json(json!({
                "result": format!("Found {} candidates; pass memoryId to delete one.", candidates.len()),
                "candidates": candidates,
            })));
        }

        if params.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
            if !params
                .get("confirm")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Ok(ToolOutput::Error(
                    "Bulk deletion requires confirm: true.".into(),
                ));
            }
            let mut qp = vec![("user_id", user_id.to_string())];
            if let Some(agent_id) = agent_id {
                qp.push(("agent_id", agent_id.to_string()));
            }
            return match self
                .inner
                .request(
                    Method::DELETE,
                    "/v1/memories/",
                    None,
                    &qp,
                    HTTP_TIMEOUT,
                    Some(&ctx.events),
                )
                .await
            {
                Ok(_) => {
                    self.inner.record_success();
                    Ok(ToolOutput::Json(json!({
                        "result": format!("All memories deleted for user \"{user_id}\"."),
                    })))
                }
                Err(e) => {
                    self.inner.record_failure();
                    Ok(ToolOutput::Error(format!("{TOOL_DELETE} failed: {e}")))
                }
            };
        }

        Err(ToolError::InvalidParams(
            "provide memoryId, query, or all: true".into(),
        ))
    }
}

struct Mem0EventListTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0EventListTool {
    fn name(&self) -> &str {
        TOOL_EVENT_LIST
    }

    fn description(&self) -> String {
        "List recent background processing events from the Mem0 Platform. Use to check whether \
         memory add / update / delete operations were processed successfully."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "required": []})
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        if self.inner.self_hosted {
            return Ok(ToolOutput::Json(json!({
                "result": "Self-hosted mem0 extracts synchronously and has no event feed.",
                "count": 0,
            })));
        }
        match self
            .inner
            .request(
                Method::GET,
                "/v1/events/",
                None,
                &[],
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(resp) => {
                self.inner.record_success();
                let items = result_items(&resp);
                if items.is_empty() {
                    return Ok(ToolOutput::Json(
                        json!({"result": "No events found.", "count": 0}),
                    ));
                }
                let events: Vec<Value> = items.iter().map(format_event).collect();
                Ok(ToolOutput::Json(json!({
                    "count": events.len(),
                    "events": events,
                })))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("{TOOL_EVENT_LIST} failed: {e}")))
            }
        }
    }
}

struct Mem0EventStatusTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0EventStatusTool {
    fn name(&self) -> &str {
        TOOL_EVENT_STATUS
    }

    fn description(&self) -> String {
        "Get detailed status of a specific Mem0 background event — whether the add / update / \
         delete was processed, its latency, and its results."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "event_id": {"type": "string", "description": "The event ID to check."}
            },
            "required": ["event_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(breaker_unavailable());
        }
        if self.inner.self_hosted {
            return Ok(ToolOutput::Error(
                "Self-hosted mem0 extracts synchronously and has no event feed.".into(),
            ));
        }
        let event_id = params
            .get("event_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("event_id is required".into()))?;
        let path = format!("/v1/event/{event_id}/");
        match self
            .inner
            .request(
                Method::GET,
                &path,
                None,
                &[],
                HTTP_TIMEOUT,
                Some(&ctx.events),
            )
            .await
        {
            Ok(resp) => {
                self.inner.record_success();
                let mut out = format_event(&resp);
                if let Some(results) = resp.get("results") {
                    out["results"] = results.clone();
                }
                Ok(ToolOutput::Json(out))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!(
                    "{TOOL_EVENT_STATUS} failed: {e}"
                )))
            }
        }
    }
}

/// Project a Mem0 event object into the compact shape both event tools surface.
fn format_event(ev: &Value) -> Value {
    json!({
        "id": ev.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
        "event_type": ev.get("event_type").and_then(|v| v.as_str()).unwrap_or_default(),
        "status": ev.get("status").and_then(|v| v.as_str()).unwrap_or_default(),
        "latency": ev.get("latency").cloned().unwrap_or(Value::Null),
        "created_at": ev.get("created_at").cloned().unwrap_or(Value::Null),
        "updated_at": ev.get("updated_at").cloned().unwrap_or(Value::Null),
    })
}

/// Parse `MemoryConfig.extra` into a typed [`Mem0Config`]. Surfaces a clear
/// error rather than allowing bad fields to silently fall through to defaults.
pub fn parse_extra(extra: &Value) -> Result<Mem0Config> {
    if extra.is_null() {
        return Ok(Mem0Config::default());
    }
    serde_json::from_value(extra.clone())
        .map_err(|e| MemoryError::Internal(anyhow!("mem0 config parse failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = Mem0Config::default();
        assert_eq!(cfg.base_url(), DEFAULT_BASE_URL);
        assert!(cfg.rerank());
        assert_eq!(cfg.top_k(), DEFAULT_TOP_K);
    }

    #[test]
    fn default_config_serializes_to_empty_object() {
        // Every field is `None` by default; `skip_serializing_if` should
        // elide each one, so the JSON written into `MemoryConfig.extra` is
        // `{}` rather than `{"api_key_name": null, "base_url": null, ...}`.
        let json = serde_json::to_value(Mem0Config::default()).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn config_round_trip() {
        let cfg = Mem0Config {
            api_key_name: Some("MY_KEY".into()),
            base_url: Some("http://localhost:9000".into()),
            rerank: Some(false),
            top_k: Some(7),
            self_hosted: Some(true),
        };
        let v = serde_json::to_value(&cfg).unwrap();
        let back: Mem0Config = serde_json::from_value(v).unwrap();
        assert_eq!(back.base_url(), "http://localhost:9000");
        assert!(!back.rerank());
        assert_eq!(back.top_k(), 7);
        assert!(back.self_hosted());
    }

    #[test]
    fn parse_extra_null_yields_defaults() {
        let cfg = parse_extra(&Value::Null).unwrap();
        assert_eq!(cfg.base_url(), DEFAULT_BASE_URL);
    }

    #[test]
    fn parse_extra_rejects_bad_types() {
        let bad = json!({"top_k": "not-a-number"});
        let err = parse_extra(&bad).unwrap_err();
        assert!(
            err.to_string().contains("config parse failed"),
            "got: {err}"
        );
    }

    #[test]
    fn missing_api_key_fails_construction() {
        let result = Mem0Memory::new(Mem0Config::default(), String::new(), None);
        match result {
            Err(e) => assert!(e.to_string().contains("API key"), "got: {e}"),
            Ok(_) => panic!("expected missing-API-key error"),
        }
    }

    #[test]
    fn self_hosted_needs_no_key_and_maps_oss_paths() {
        let cfg = Mem0Config {
            self_hosted: Some(true),
            base_url: Some("http://host:8000".into()),
            ..Default::default()
        };
        // No key required in self-hosted mode.
        let mem = Mem0Memory::new(cfg, String::new(), None).expect("self-hosted needs no key");
        let inner = &mem.inner;
        assert!(inner.self_hosted);
        // Platform paths fold onto the OSS server's shapes.
        assert_eq!(inner.map_path("/v1/memories/"), "/memories");
        assert_eq!(inner.map_path("/v2/memories/search/"), "/search");
        assert_eq!(inner.map_path("/v2/memories/"), "/memories");
        assert_eq!(inner.map_path("/v1/memories/abc123/"), "/memories/abc123");
        assert_eq!(inner.url("/v2/memories/search/"), "http://host:8000/search");
    }

    #[test]
    fn platform_mode_leaves_paths_untouched() {
        let mem = Mem0Memory::new(Mem0Config::default(), "k".into(), None).unwrap();
        assert!(!mem.inner.self_hosted);
        assert_eq!(
            mem.inner.map_path("/v2/memories/search/"),
            "/v2/memories/search/"
        );
        assert_eq!(
            mem.inner.url("/v1/memories/"),
            "https://api.mem0.ai/v1/memories/"
        );
    }

    #[test]
    fn concat_text_skips_non_text() {
        use baybo_model::BlobRef;
        let blocks = vec![
            ContentBlock::Text("hello".into()),
            ContentBlock::Image {
                blob: BlobRef {
                    blob_id: "sha256:0".into(),
                },
                mime_type: "image/png".into(),
                filename: None,
            },
            ContentBlock::Text("world".into()),
        ];
        assert_eq!(concat_text(&blocks), "hello\nworld");
    }

    #[test]
    fn concat_text_empty_for_no_text() {
        let blocks: Vec<ContentBlock> = vec![];
        assert_eq!(concat_text(&blocks), "");
    }

    #[test]
    fn parse_search_results_handles_wrapped_and_bare_lists() {
        let bare = json!([
            {"memory": "fact one", "score": 0.9},
            {"memory": "fact two", "score": 0.8}
        ]);
        let out = parse_search_results(&bare);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "fact one");

        let wrapped = json!({"results": [{"memory": "x"}]});
        let out = parse_search_results(&wrapped);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "x");

        let empty = json!({});
        assert!(parse_search_results(&empty).is_empty());
    }

    #[test]
    fn build_filters_wraps_user_and_optional_scopes() {
        let f = build_filters("u-1", None, None, None, None);
        assert_eq!(f["AND"][0]["user_id"], "u-1");

        let scoped = build_filters("u-1", Some("a-1"), Some("s-1"), None, None);
        assert_eq!(scoped["AND"][0]["user_id"], "u-1");
        assert_eq!(scoped["AND"][1]["agent_id"], "a-1");
        assert_eq!(scoped["AND"][2]["run_id"], "s-1");

        let cats = vec!["identity".to_string()];
        let with_cats = build_filters("u-1", None, None, Some(&cats), None);
        assert_eq!(with_cats["AND"][1]["categories"]["in"][0], "identity");
    }

    #[test]
    fn build_filters_passes_through_prebuilt_and_or() {
        let prebuilt = json!({"OR": [{"user_id": "a"}, {"user_id": "b"}]});
        let f = build_filters("ignored", None, None, None, Some(&prebuilt));
        assert_eq!(f, prebuilt);
    }

    #[test]
    fn scope_run_id_only_for_session() {
        assert_eq!(scope_run_id(Some("session"), "s-1"), Some("s-1"));
        assert_eq!(scope_run_id(Some("long-term"), "s-1"), None);
        assert_eq!(scope_run_id(Some("all"), "s-1"), None);
        assert_eq!(scope_run_id(None, "s-1"), None);
    }

    #[test]
    fn breaker_trips_then_cools_down() {
        let mut s = BreakerState::default();
        assert!(!s.is_open());
        for _ in 0..BREAKER_THRESHOLD {
            s.record_failure();
        }
        assert!(s.is_open());
        s.record_success();
        assert!(!s.is_open(), "success resets the breaker");
    }
}
