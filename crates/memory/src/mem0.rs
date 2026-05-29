//! Mem0 Platform memory backend.
//!
//! Hosted SaaS memory with server-side LLM fact extraction, semantic search,
//! and reranking via the Mem0 REST API.
//!
//! - **`recall`**: `POST /v2/memories/search` with `{filters, rerank, top_k}`.
//! - **`on_job_complete`**: `POST /v1/memories` with `{messages, user_id, agent_id}`.
//! - **`on_session_end`**: no-op (Mem0 has no session concept; extraction is
//!   immediate on `add`).
//!
//! Per-user scoping comes from `MemoryContext::user_id()` at every call;
//! `agent_id` is config-level (deployment identity).
//!
//! Failures are routed through a 5-failure / 120 s circuit breaker that pauses
//! API calls after sustained outages (port from the Python `_record_failure` /
//! `_breaker_open_until` logic).
//!
//! # References
//!
//! - Mem0 project: <https://github.com/mem0ai/mem0>
//! - Mem0 Platform API docs: <https://docs.mem0.ai/api-reference>
//! - Reference Python implementation (hermes-agent plugin, pinned):
//!   <https://github.com/NousResearch/hermes-agent/blob/678a87c47753a98ab2320def830c7ae24cda4c0e/plugins/memory/mem0/__init__.py>
//! - Original hermes-agent PR (Mem0 integration): <https://github.com/NousResearch/hermes-agent/pull/2933>

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use async_trait::async_trait;
use aura_model::{ContentBlock, TrustLevel};
use aura_security::SecretVault;
use aura_security::http::ProxySettings;
use aura_tools::{Tool, ToolCapability, ToolContext, ToolManifest, ToolOutput};
use parking_lot::Mutex;
use reqwest::header;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::{Memory, MemoryContext, MemoryError, RecalledMemory, Result};

const DEFAULT_BASE_URL: &str = "https://api.mem0.ai";
const DEFAULT_AGENT_ID: &str = "aura";
const DEFAULT_TOP_K: usize = 5;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Probe is best-effort and runs inline during `build_managers`; keep the
/// per-request budget short so an unreachable Mem0 endpoint does not stall
/// boot up to `HTTP_TIMEOUT` (mirrors openviking's `HEALTH_TIMEOUT`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const VAULT_KEY: &str = "memory.mem0.api_key";
const DEFAULT_API_KEY_ENV: &str = "MEM0_API_KEY";

/// Tool names. Exposed as constants so the runtime / tests can reference them
/// without literal-typo risk.
pub const TOOL_PROFILE: &str = "mem0_profile";
pub const TOOL_SEARCH: &str = "mem0_search";
pub const TOOL_CONCLUDE: &str = "mem0_conclude";

const BREAKER_THRESHOLD: u32 = 5;
const BREAKER_COOLDOWN: Duration = Duration::from_secs(120);

const MAX_PROFILE_ENTRIES: usize = 100;

/// Per-backend config deserialized from `MemoryConfig.extra`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Mem0Config {
    /// Explicit env var holding the API key. When unset, the runtime falls
    /// back to the vault key `memory.mem0.api_key` and then `MEM0_API_KEY`.
    pub api_key_env: Option<String>,
    /// Override the Mem0 REST base URL. `None` → `https://api.mem0.ai`.
    pub base_url: Option<String>,
    /// `agent_id` tag attached to writes (`POST /v1/memories`). Stable per
    /// deployment; per-user scope rides on `MemoryContext::user_id()`.
    pub agent_id: Option<String>,
    /// Enable Mem0 server-side reranking for `recall` (more accurate, slower).
    pub rerank: Option<bool>,
    /// Max results returned by `recall`.
    pub top_k: Option<usize>,
}

impl Mem0Config {
    fn agent_id(&self) -> &str {
        self.agent_id.as_deref().unwrap_or(DEFAULT_AGENT_ID)
    }

    fn base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)
    }

    fn rerank(&self) -> bool {
        self.rerank.unwrap_or(true)
    }

    fn top_k(&self) -> usize {
        self.top_k.unwrap_or(DEFAULT_TOP_K)
    }
}

/// Resolve the Mem0 API key. Order:
///   1. Explicit env var named by `cfg.api_key_env`.
///   2. Per-entry vault key `memory.mem0.api_key`.
///   3. Default env var `MEM0_API_KEY`.
///
/// Mirrors `aura_llm::credentials::resolve_api_key` exactly; kept local
/// because the vault key + default env are memory-specific.
pub async fn resolve_api_key(cfg: &Mem0Config, vault: Option<&SecretVault>) -> Option<String> {
    if let Some(env) = &cfg.api_key_env
        && let Ok(v) = std::env::var(env)
    {
        return Some(v);
    }
    if let Some(vault) = vault
        && let Ok(Some(secret)) = vault.get_secret(VAULT_KEY).await
        && let Ok(s) = secret.as_str()
    {
        return Some(s.to_string());
    }
    std::env::var(DEFAULT_API_KEY_ENV).ok()
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
    agent_id: String,
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
        format!("{base}{path}")
    }

    fn auth_header(&self) -> String {
        format!("Token {}", self.api_key)
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        let resp = self
            .client
            .post(self.url(path))
            .header(header::AUTHORIZATION, self.auth_header())
            .json(body)
            .send()
            .await
            .map_err(|e| MemoryError::Backend(format!("mem0 request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Backend(format!(
                "mem0 {path} returned {status}: {body}"
            )));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| MemoryError::Backend(format!("mem0 response parse failed: {e}")))
    }
}

/// Mem0 memory backend. Construct via [`Mem0Memory::new`] inside
/// `runtime.rs::build_managers`.
pub struct Mem0Memory {
    inner: Arc<Mem0Inner>,
}

impl Mem0Memory {
    /// Build the backend from typed config + resolved API key. The
    /// `extra` blob from [`aura_config::MemoryConfig`] is parsed into a
    /// [`Mem0Config`] by the caller (see [`Mem0Config`] / `serde_json::from_value`).
    /// `proxy` threads the deployment-configured egress proxy through
    /// [`aura_security::http::client_builder`] — the crate-wide outbound
    /// chokepoint.
    pub fn new(cfg: Mem0Config, api_key: String, proxy: Option<&ProxySettings>) -> Result<Self> {
        if api_key.is_empty() {
            return Err(MemoryError::Backend(
                "mem0 API key missing — set api_key_env, MEM0_API_KEY, or vault \
                 key memory.mem0.api_key"
                    .into(),
            ));
        }
        let client = aura_security::http::client_builder(proxy)
            .map_err(|e| MemoryError::Backend(format!("mem0 client build failed: {e}")))?
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| MemoryError::Backend(format!("mem0 client build failed: {e}")))?;
        let inner = Arc::new(Mem0Inner {
            client,
            base_url: cfg.base_url().to_string(),
            api_key,
            agent_id: cfg.agent_id().to_string(),
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
            "filters": {"AND": [{"user_id": "__aura_probe__"}]},
            "page": 1,
            "page_size": 1,
        });
        match tokio::time::timeout(PROBE_TIMEOUT, self.inner.post_json("/v2/memories", &body)).await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(error = %e, "mem0 startup probe failed; continuing"),
            Err(_) => warn!(
                timeout_secs = PROBE_TIMEOUT.as_secs(),
                "mem0 startup probe timed out; continuing"
            ),
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

fn read_filters(user_id: &str) -> Value {
    json!({"AND": [{"user_id": user_id}]})
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
        let q = concat_text(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({
            "query": q,
            "filters": read_filters(ctx.user_id()),
            "rerank": self.inner.rerank,
            "top_k": self.inner.top_k,
        });
        match self.inner.post_json("/v2/memories/search", &body).await {
            Ok(resp) => {
                self.inner.record_success();
                Ok(parse_search_results(&resp))
            }
            Err(e) => {
                self.inner.record_failure();
                warn!(error = %e, "mem0 recall failed");
                Ok(Vec::new())
            }
        }
    }

    async fn on_job_complete(
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
        let body = json!({
            "messages": [
                {"role": "user", "content": user_text},
                {"role": "assistant", "content": assistant_text},
            ],
            "user_id": ctx.user_id(),
            "agent_id": self.inner.agent_id,
        });
        match self.inner.post_json("/v1/memories", &body).await {
            Ok(_) => {
                self.inner.record_success();
                Ok(())
            }
            Err(e) => {
                self.inner.record_failure();
                warn!(error = %e, "mem0 on_job_complete failed");
                Ok(())
            }
        }
    }

    async fn on_session_end(
        &self,
        _ctx: &MemoryContext,
        _transcript: &[aura_model::ChatMessage],
    ) -> Result<()> {
        Ok(())
    }

    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
        let inner = Arc::clone(&self.inner);
        vec![
            tool_pair(Arc::new(Mem0ProfileTool {
                inner: Arc::clone(&inner),
            })),
            tool_pair(Arc::new(Mem0SearchTool {
                inner: Arc::clone(&inner),
            })),
            tool_pair(Arc::new(Mem0ConcludeTool { inner })),
        ]
    }
}

fn parse_search_results(resp: &Value) -> Vec<RecalledMemory> {
    let items: &[Value] = if let Some(arr) = resp.as_array() {
        arr
    } else if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
        arr
    } else {
        return Vec::new();
    };
    items
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
    };
    (tool, manifest)
}

// ---------------------------------------------------------------------------
// Tools — `mem0_profile`, `mem0_search`, `mem0_conclude`.
// ---------------------------------------------------------------------------

struct Mem0ProfileTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0ProfileTool {
    fn name(&self) -> &str {
        TOOL_PROFILE
    }

    fn description(&self) -> String {
        "Retrieve all stored memories about the user — preferences, facts, project context. \
         Fast, no reranking. Use at conversation start."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "required": []})
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(ToolOutput::Error(
                "Mem0 API temporarily unavailable (circuit breaker tripped). \
                 Will retry automatically."
                    .into(),
            ));
        }
        let body = json!({
            "filters": read_filters(ctx.user.id.as_str()),
            "page": 1,
            "page_size": MAX_PROFILE_ENTRIES,
        });
        match self.inner.post_json("/v2/memories", &body).await {
            Ok(resp) => {
                self.inner.record_success();
                let memories = parse_search_results(&resp);
                if memories.is_empty() {
                    Ok(ToolOutput::Json(
                        json!({"result": "No memories stored yet."}),
                    ))
                } else {
                    let lines: Vec<&str> = memories.iter().map(|m| m.content.as_str()).collect();
                    let more = lines.len() == MAX_PROFILE_ENTRIES;
                    Ok(ToolOutput::Json(json!({
                        "result": lines.join("\n"),
                        "count": lines.len(),
                        "truncated": more,
                        "hint": if more { "More available — use mem0_search to query." } else { "" },
                    })))
                }
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("mem0_profile failed: {e}")))
            }
        }
    }
}

struct Mem0SearchTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0SearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH
    }

    fn description(&self) -> String {
        "Search memories by meaning. Returns relevant facts ranked by similarity. \
         Set rerank=true for higher accuracy on important queries."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for."},
                "rerank": {"type": "boolean", "description": "Enable reranking for precision (default: false)."},
                "top_k": {"type": "integer", "description": "Max results (default: 10, max: 50)."}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(ToolOutput::Error(
                "Mem0 API temporarily unavailable (circuit breaker tripped).".into(),
            ));
        }
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| aura_tools::ToolError::InvalidParams("query is required".into()))?;
        let rerank = params
            .get("rerank")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let top_k = params
            .get("top_k")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(50) as usize)
            .unwrap_or(10);

        let body = json!({
            "query": query,
            "filters": read_filters(ctx.user.id.as_str()),
            "rerank": rerank,
            "top_k": top_k,
        });
        match self.inner.post_json("/v2/memories/search", &body).await {
            Ok(resp) => {
                self.inner.record_success();
                let items: Vec<Value> = resp
                    .as_array()
                    .cloned()
                    .or_else(|| resp.get("results").and_then(|v| v.as_array()).cloned())
                    .unwrap_or_default();
                if items.is_empty() {
                    return Ok(ToolOutput::Json(
                        json!({"result": "No relevant memories found."}),
                    ));
                }
                let formatted: Vec<Value> = items
                    .iter()
                    .map(|m| {
                        json!({
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
                Ok(ToolOutput::Error(format!("mem0_search failed: {e}")))
            }
        }
    }
}

struct Mem0ConcludeTool {
    inner: Arc<Mem0Inner>,
}

#[async_trait]
impl Tool for Mem0ConcludeTool {
    fn name(&self) -> &str {
        TOOL_CONCLUDE
    }

    fn description(&self) -> String {
        "Store a durable fact about the user. Stored verbatim (no LLM extraction). \
         Use for explicit preferences, corrections, or decisions."
            .into()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "conclusion": {"type": "string", "description": "The fact to store."}
            },
            "required": ["conclusion"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        if self.inner.breaker_open() {
            return Ok(ToolOutput::Error(
                "Mem0 API temporarily unavailable (circuit breaker tripped).".into(),
            ));
        }
        let conclusion = params
            .get("conclusion")
            .and_then(|v| v.as_str())
            .ok_or_else(|| aura_tools::ToolError::InvalidParams("conclusion is required".into()))?;
        if conclusion.is_empty() {
            return Err(aura_tools::ToolError::InvalidParams(
                "conclusion cannot be empty".into(),
            ));
        }
        let body = json!({
            "messages": [{"role": "user", "content": conclusion}],
            "user_id": ctx.user.id.as_str(),
            "agent_id": self.inner.agent_id,
            "infer": false,
        });
        match self.inner.post_json("/v1/memories", &body).await {
            Ok(_) => {
                self.inner.record_success();
                Ok(ToolOutput::Json(json!({"result": "Fact stored."})))
            }
            Err(e) => {
                self.inner.record_failure();
                Ok(ToolOutput::Error(format!("mem0_conclude failed: {e}")))
            }
        }
    }
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
        assert_eq!(cfg.agent_id(), DEFAULT_AGENT_ID);
        assert!(cfg.rerank());
        assert_eq!(cfg.top_k(), DEFAULT_TOP_K);
    }

    #[test]
    fn config_round_trip() {
        let cfg = Mem0Config {
            api_key_env: Some("MY_KEY".into()),
            base_url: Some("http://localhost:9000".into()),
            agent_id: Some("test".into()),
            rerank: Some(false),
            top_k: Some(7),
        };
        let v = serde_json::to_value(&cfg).unwrap();
        let back: Mem0Config = serde_json::from_value(v).unwrap();
        assert_eq!(back.base_url(), "http://localhost:9000");
        assert_eq!(back.agent_id(), "test");
        assert!(!back.rerank());
        assert_eq!(back.top_k(), 7);
    }

    #[test]
    fn parse_extra_null_yields_defaults() {
        let cfg = parse_extra(&Value::Null).unwrap();
        assert_eq!(cfg.agent_id(), DEFAULT_AGENT_ID);
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
    fn concat_text_skips_non_text() {
        use aura_model::BlobRef;
        let blocks = vec![
            ContentBlock::Text("hello".into()),
            ContentBlock::Image {
                blob: BlobRef {
                    blob_id: "sha256:0".into(),
                },
                mime_type: "image/png".into(),
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
