//! OpenViking memory backend.
//!
//! Self-hosted context database (Volcengine / ByteDance) that organizes agent
//! knowledge into a filesystem hierarchy under `viking://` URIs with tiered
//! context loading (abstract / overview / full) and server-side memory
//! extraction.
//!
//! - **`recall`**: `POST /api/v1/search/find` with `{query, top_k}`.
//! - **`on_job_complete`**: `POST /api/v1/sessions/{ctx.session_id}/messages`
//!   ×2 (user + assistant). The server accumulates session state for the
//!   eventual commit.
//! - **`on_session_end`**: `POST /api/v1/sessions/{ctx.session_id}/commit`,
//!   skipped when `transcript.is_empty()`. Triggers the 6-category server-side
//!   extraction (preferences / entities / events / cases / patterns / profile).
//!
//! Five tools: `viking_search`, `viking_read`, `viking_browse`,
//! `viking_remember`, `viking_add_resource`.
//!
//! # References
//!
//! - OpenViking project: <https://github.com/volcengine/openviking>
//! - Reference Python implementation (hermes-agent plugin, pinned):
//!   <https://github.com/NousResearch/hermes-agent/blob/d617858896788aaddce77746e3bd890b3e75124c/plugins/memory/openviking/__init__.py>
//! - Original hermes-agent PR (OpenViking integration): <https://github.com/NousResearch/hermes-agent/pull/3369>

use std::fs::File;
use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use aura_model::{ChatMessage, ContentBlock, TrustLevel};
use aura_security::SecretVault;
use aura_security::http::ProxySettings;
use aura_tools::{Tool, ToolCapability, ToolContext, ToolManifest, ToolOutput};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;
use walkdir::WalkDir;
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use crate::{Memory, MemoryContext, MemoryError, RecalledMemory, Result};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:1933";
const DEFAULT_ACCOUNT: &str = "default";
const DEFAULT_AGENT: &str = "aura";
const DEFAULT_TOP_K: usize = 5;
/// Default per-request budget for tool calls (model is already waiting).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// Recall is on the critical path — cap it well below `HTTP_TIMEOUT` so a
/// slow / down OpenViking server degrades the turn to "no recalled context"
/// instead of stalling the user up to 30 s.
const RECALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Background-write budget for `on_job_complete` / `on_session_end`.
/// Detached on the runtime root, so the user never waits;
/// `/sessions/{sid}/commit` triggers the 6-category server-side extraction
/// (LLM-backed), which can be slow under load — give it a generous ceiling.
const WRITE_TIMEOUT: Duration = Duration::from_secs(600);
const VAULT_KEY: &str = "memory.openviking.api_key";
const DEFAULT_API_KEY_ENV: &str = "OPENVIKING_API_KEY";

const REMOTE_PREFIXES: &[&str] = &["http://", "https://", "git@", "ssh://", "git://"];

/// Cap on a single local upload — matches `aura_tools::builtin::send_local_file`
/// (the only other tool that ships local bytes to a remote endpoint). Applies
/// to both single-file uploads and the zipped-directory path (post-deflation).
const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

pub const TOOL_SEARCH: &str = "viking_search";
pub const TOOL_READ: &str = "viking_read";
pub const TOOL_BROWSE: &str = "viking_browse";
pub const TOOL_REMEMBER: &str = "viking_remember";
pub const TOOL_ADD_RESOURCE: &str = "viking_add_resource";

const DEFAULT_MEMORY_SUBDIR: &str = "preferences";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OpenVikingConfig {
    /// Override the OpenViking REST endpoint. `None` →
    /// `http://127.0.0.1:1933` (local dev default).
    pub endpoint: Option<String>,
    /// Explicit env var holding the API key. When unset, the runtime falls
    /// back to the vault key `memory.openviking.api_key` and then
    /// `OPENVIKING_API_KEY`. Leave the key empty for unauthenticated local
    /// dev servers.
    pub api_key_env: Option<String>,
    /// `X-OpenViking-Account` header (tenant identity). Per-user scope is the
    /// `MemoryContext::user_id()` at each call, sent as `X-OpenViking-User`.
    pub account: Option<String>,
    /// `X-OpenViking-Agent` header — which agent is writing. Defaults to
    /// `"aura"`.
    pub agent: Option<String>,
    /// Max results returned by `recall`.
    pub top_k: Option<usize>,
}

impl OpenVikingConfig {
    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(DEFAULT_ENDPOINT)
    }
    fn account(&self) -> &str {
        self.account.as_deref().unwrap_or(DEFAULT_ACCOUNT)
    }
    fn agent(&self) -> &str {
        self.agent.as_deref().unwrap_or(DEFAULT_AGENT)
    }
    fn top_k(&self) -> usize {
        self.top_k.unwrap_or(DEFAULT_TOP_K)
    }
}

/// Resolve the OpenViking API key. Order: explicit env, vault, default env.
/// Returns `Ok(String::new())` when no key is set anywhere — OpenViking local
/// dev mode runs without authentication.
pub async fn resolve_api_key(cfg: &OpenVikingConfig, vault: Option<&SecretVault>) -> String {
    if let Some(env) = &cfg.api_key_env
        && let Ok(v) = std::env::var(env)
    {
        return v;
    }
    if let Some(vault) = vault
        && let Ok(Some(secret)) = vault.get_secret(VAULT_KEY).await
        && let Ok(s) = secret.as_str()
    {
        return s.to_string();
    }
    std::env::var(DEFAULT_API_KEY_ENV).unwrap_or_default()
}

struct OpenVikingInner {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    account: String,
    agent: String,
    top_k: usize,
}

impl OpenVikingInner {
    fn url(&self, path: &str) -> String {
        let base = self.endpoint.trim_end_matches('/');
        format!("{base}{path}")
    }

    fn base_headers(&self, user_id: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&self.account) {
            h.insert(HeaderName::from_static("x-openviking-account"), v);
        }
        if let Ok(v) = HeaderValue::from_str(user_id) {
            h.insert(HeaderName::from_static("x-openviking-user"), v);
        }
        if let Ok(v) = HeaderValue::from_str(&self.agent) {
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

    fn json_headers(&self, user_id: &str) -> HeaderMap {
        let mut h = self.base_headers(user_id);
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h
    }

    async fn parse(&self, resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
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

    async fn get(
        &self,
        path: &str,
        user_id: &str,
        query: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Value> {
        let url = build_url(&self.endpoint, path, query)
            .map_err(|e| MemoryError::Backend(format!("openviking url build failed: {e}")))?;
        let resp = self
            .client
            .get(url)
            .headers(self.base_headers(user_id))
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| MemoryError::Backend(format!("openviking GET {path} failed: {e}")))?;
        self.parse(resp).await
    }

    async fn post_json(
        &self,
        path: &str,
        user_id: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let resp = self
            .client
            .post(self.url(path))
            .headers(self.json_headers(user_id))
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| MemoryError::Backend(format!("openviking POST {path} failed: {e}")))?;
        self.parse(resp).await
    }

    async fn post_empty(&self, path: &str, user_id: &str, timeout: Duration) -> Result<Value> {
        let resp = self
            .client
            .post(self.url(path))
            .headers(self.json_headers(user_id))
            .body("{}")
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| MemoryError::Backend(format!("openviking POST {path} failed: {e}")))?;
        self.parse(resp).await
    }

    async fn upload_temp_file(&self, user_id: &str, file_path: &Path) -> Result<String> {
        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| MemoryError::Backend(format!("read upload file failed: {e}")))?;
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("upload.bin")
            .to_string();
        let mime = mime_for(&file_name);
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(&mime)
            .map_err(|e| MemoryError::Backend(format!("mime: {e}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let resp = self
            .client
            .post(self.url("/api/v1/resources/temp_upload"))
            .headers(self.base_headers(user_id))
            .multipart(form)
            .send()
            .await
            .map_err(|e| MemoryError::Backend(format!("openviking temp_upload failed: {e}")))?;
        let body = self.parse(resp).await?;
        body.get("result")
            .and_then(|r| r.get("temp_file_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                MemoryError::Backend("openviking temp_upload missing temp_file_id".into())
            })
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

fn mime_for(name: &str) -> String {
    let lower = name.to_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext {
        "json" => "application/json",
        "zip" => "application/zip",
        "txt" | "md" | "log" | "rs" | "py" | "ts" | "js" | "rb" | "go" | "sh" => "text/plain",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "html" | "htm" => "text/html",
        _ => "application/octet-stream",
    }
    .into()
}

/// OpenViking memory backend. Construct via [`OpenVikingMemory::new`].
///
/// Unlike `mem0` this backend ships no circuit breaker (matches the Python
/// reference) — the default endpoint is loopback / local dev, where a
/// dropped server during iteration would otherwise pin the breaker open for
/// the cooldown window. Failures are logged at `warn`/`debug` and swallowed
/// per call.
pub struct OpenVikingMemory {
    inner: Arc<OpenVikingInner>,
}

impl OpenVikingMemory {
    /// Build the backend. `proxy` is threaded into the underlying
    /// `reqwest::Client` via [`aura_security::http::client_builder`] — the
    /// crate-wide outbound-egress chokepoint — so a deployment-configured
    /// proxy applies to recall/write/tool traffic. `ALWAYS_DIRECT`
    /// (`localhost,127.0.0.1,::1`) is honoured, so the default loopback
    /// endpoint stays direct.
    pub fn new(
        cfg: OpenVikingConfig,
        api_key: String,
        proxy: Option<&ProxySettings>,
    ) -> Result<Self> {
        let client = aura_security::http::client_builder(proxy)
            .map_err(|e| MemoryError::Backend(format!("openviking client build failed: {e}")))?
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| MemoryError::Backend(format!("openviking client build failed: {e}")))?;
        let inner = Arc::new(OpenVikingInner {
            client,
            endpoint: cfg.endpoint().trim_end_matches('/').to_string(),
            api_key,
            account: cfg.account().to_string(),
            agent: cfg.agent().to_string(),
            top_k: cfg.top_k(),
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
            .headers(self.inner.base_headers("default"))
            .timeout(HEALTH_TIMEOUT)
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

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

fn build_memory_uri(user_id: &str, subdir: &str) -> String {
    let slug = Uuid::new_v4().simple().to_string();
    let slug = &slug[..slug.len().min(12)];
    format!("viking://user/{user_id}/memories/{subdir}/mem_{slug}.md")
}

fn category_to_subdir(category: &str) -> &'static str {
    match category {
        "preference" => "preferences",
        "entity" => "entities",
        "event" => "events",
        "case" => "cases",
        "pattern" => "patterns",
        _ => DEFAULT_MEMORY_SUBDIR,
    }
}

#[async_trait]
impl Memory for OpenVikingMemory {
    async fn recall(
        &self,
        ctx: &MemoryContext,
        query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>> {
        let q = concat_text(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({"query": q, "top_k": self.inner.top_k});
        match self
            .inner
            .post_json("/api/v1/search/find", ctx.user_id(), &body, RECALL_TIMEOUT)
            .await
        {
            Ok(resp) => Ok(parse_search_results(&resp)),
            Err(e) => {
                warn!(error = %e, "openviking recall failed (timeout or backend)");
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
        let user_text = concat_text(user_input);
        let assistant_text = concat_text(final_output);
        if user_text.is_empty() && assistant_text.is_empty() {
            return Ok(());
        }
        let sid = ctx.session_id().as_str();
        let path = format!("/api/v1/sessions/{sid}/messages");
        let user_id = ctx.user_id();
        if !user_text.is_empty() {
            let body = json!({"role": "user", "content": truncate_str(&user_text, 4000)});
            if let Err(e) = self
                .inner
                .post_json(&path, user_id, &body, WRITE_TIMEOUT)
                .await
            {
                debug!(error = %e, "openviking on_job_complete user msg failed");
            }
        }
        if !assistant_text.is_empty() {
            let body = json!({"role": "assistant", "content": truncate_str(&assistant_text, 4000)});
            if let Err(e) = self
                .inner
                .post_json(&path, user_id, &body, WRITE_TIMEOUT)
                .await
            {
                debug!(error = %e, "openviking on_job_complete assistant msg failed");
            }
        }
        Ok(())
    }

    async fn on_session_end(&self, ctx: &MemoryContext, transcript: &[ChatMessage]) -> Result<()> {
        if transcript.is_empty() {
            return Ok(());
        }
        let sid = ctx.session_id().as_str();
        let path = format!("/api/v1/sessions/{sid}/commit");
        if let Err(e) = self
            .inner
            .post_empty(&path, ctx.user_id(), WRITE_TIMEOUT)
            .await
        {
            warn!(error = %e, "openviking session commit failed");
        }
        Ok(())
    }

    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
        let inner = Arc::clone(&self.inner);
        vec![
            tool_pair(Arc::new(VikingSearchTool {
                inner: Arc::clone(&inner),
            })),
            tool_pair(Arc::new(VikingReadTool {
                inner: Arc::clone(&inner),
            })),
            tool_pair(Arc::new(VikingBrowseTool {
                inner: Arc::clone(&inner),
            })),
            tool_pair(Arc::new(VikingRememberTool {
                inner: Arc::clone(&inner),
            })),
            // `viking_add_resource` also reads the local filesystem when handed
            // a local URL; declare both capabilities for governance accuracy.
            tool_pair_with_caps(
                Arc::new(VikingAddResourceTool { inner }),
                vec![ToolCapability::Http, ToolCapability::ReadFile],
            ),
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
    tool_pair_with_caps(tool, vec![ToolCapability::Http])
}

fn tool_pair_with_caps(
    tool: Arc<dyn Tool>,
    capabilities: Vec<ToolCapability>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities,
    };
    (tool, manifest)
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

struct VikingSearchTool {
    inner: Arc<OpenVikingInner>,
}

#[async_trait]
impl Tool for VikingSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH
    }
    fn description(&self) -> String {
        "Semantic search over the OpenViking knowledge base. Returns ranked results with \
         viking:// URIs for deeper reading. Use mode='deep' for complex queries that need \
         reasoning across multiple sources, 'fast' for simple lookups."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query."},
                "mode": {
                    "type": "string", "enum": ["auto", "fast", "deep"],
                    "description": "Search depth (default: auto)."
                },
                "scope": {
                    "type": "string",
                    "description": "Viking URI prefix to scope search (e.g. 'viking://resources/docs/')."
                },
                "limit": {"type": "integer", "description": "Max results (default: 10)."}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| aura_tools::ToolError::InvalidParams("query is required".into()))?;
        let mut body = json!({"query": query});
        if let Some(mode) = params.get("mode").and_then(|v| v.as_str())
            && mode != "auto"
        {
            body["mode"] = json!(mode);
        }
        if let Some(scope) = params.get("scope").and_then(|v| v.as_str()) {
            body["target_uri"] = json!(scope);
        }
        if let Some(limit) = params.get("limit").and_then(|v| v.as_u64()) {
            body["top_k"] = json!(limit);
        }

        let resp = self
            .inner
            .post_json(
                "/api/v1/search/find",
                ctx.user.id.as_str(),
                &body,
                HTTP_TIMEOUT,
            )
            .await
            .map_err(|e| aura_tools::ToolError::Execution(e.to_string()))?;
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        let mut scored: Vec<(f64, Value)> = Vec::new();
        for ctx_type in ["memories", "resources", "skills"] {
            if let Some(items) = result.get(ctx_type).and_then(|v| v.as_array()) {
                for item in items {
                    let score = item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let entry = json!({
                        "uri": item.get("uri").and_then(|v| v.as_str()).unwrap_or(""),
                        "type": ctx_type.trim_end_matches('s'),
                        "score": (score * 1000.0).round() / 1000.0,
                        "abstract": item.get("abstract").and_then(|v| v.as_str()).unwrap_or(""),
                    });
                    scored.push((score, entry));
                }
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let formatted: Vec<Value> = scored.into_iter().map(|(_, v)| v).collect();
        let total = result
            .get("total")
            .and_then(|v| v.as_u64())
            .unwrap_or(formatted.len() as u64);
        Ok(ToolOutput::Json(
            json!({"results": formatted, "total": total}),
        ))
    }
}

struct VikingReadTool {
    inner: Arc<OpenVikingInner>,
}

#[async_trait]
impl Tool for VikingReadTool {
    fn name(&self) -> &str {
        TOOL_READ
    }
    fn description(&self) -> String {
        "Read content at a viking:// URI. Three detail levels:\n\
         abstract — ~100 token summary (L0)\n\
         overview — ~2k token key points (L1)\n\
         full — complete content (L2)\n\
         Start with abstract/overview, only use full when you need details."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string", "description": "viking:// URI to read."},
                "level": {
                    "type": "string", "enum": ["abstract", "overview", "full"],
                    "description": "Detail level (default: overview)."
                }
            },
            "required": ["uri"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| aura_tools::ToolError::InvalidParams("uri is required".into()))?
            .to_string();
        let level = params
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("overview")
            .to_string();
        let summary_level = level == "abstract" || level == "overview";
        let resolved = if summary_level {
            normalize_summary_uri(&uri)
        } else {
            uri.clone()
        };

        let endpoint = match level.as_str() {
            "abstract" => "/api/v1/content/abstract",
            "overview" => "/api/v1/content/overview",
            _ => "/api/v1/content/read",
        };

        let user_id = ctx.user.id.as_str();
        let mut used_fallback = false;
        let resp = match self
            .inner
            .get(endpoint, user_id, &[("uri", &resolved)], HTTP_TIMEOUT)
            .await
        {
            Ok(v) => v,
            Err(e) if summary_level => {
                used_fallback = true;
                debug!(error = %e, "openviking summary endpoint failed; falling back to /content/read");
                self.inner
                    .get(
                        "/api/v1/content/read",
                        user_id,
                        &[("uri", &uri)],
                        HTTP_TIMEOUT,
                    )
                    .await
                    .map_err(|e| aura_tools::ToolError::Execution(e.to_string()))?
            }
            Err(e) => return Err(aura_tools::ToolError::Execution(e.to_string())),
        };

        let result = resp.get("result").cloned().unwrap_or(resp);
        let content = match &result {
            Value::String(s) => s.clone(),
            Value::Object(_) => result
                .get("content")
                .and_then(|v| v.as_str())
                .or_else(|| result.get("text").and_then(|v| v.as_str()))
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };

        let max_len = match level.as_str() {
            "abstract" => 1200,
            "overview" => 4000,
            _ => 8000,
        };
        let trimmed = if content.len() > max_len {
            let mut t = truncate_str(&content, max_len);
            t.push_str("\n\n[... truncated, use a more specific URI or full level]");
            t
        } else {
            content
        };

        let mut payload = json!({
            "uri": uri,
            "resolved_uri": resolved,
            "level": level,
            "content": trimmed,
        });
        if used_fallback {
            payload["fallback"] = json!("content/read");
        }
        Ok(ToolOutput::Json(payload))
    }
}

fn normalize_summary_uri(uri: &str) -> String {
    for suffix in ["/.abstract.md", "/.overview.md", "/.read.md", "/.full.md"] {
        if let Some(stripped) = uri.strip_suffix(suffix) {
            return if stripped.is_empty() {
                "viking://".to_string()
            } else {
                stripped.to_string()
            };
        }
    }
    uri.to_string()
}

struct VikingBrowseTool {
    inner: Arc<OpenVikingInner>,
}

#[async_trait]
impl Tool for VikingBrowseTool {
    fn name(&self) -> &str {
        TOOL_BROWSE
    }
    fn description(&self) -> String {
        "Browse the OpenViking knowledge store like a filesystem.\n\
         list — show directory contents\n\
         tree — show hierarchy\n\
         stat — show metadata for a URI"
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string", "enum": ["tree", "list", "stat"],
                    "description": "Browse action."
                },
                "path": {
                    "type": "string",
                    "description": "Viking URI path (default: viking://)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list")
            .to_string();
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("viking://")
            .to_string();
        let endpoint = match action.as_str() {
            "tree" => "/api/v1/fs/tree",
            "stat" => "/api/v1/fs/stat",
            _ => "/api/v1/fs/ls",
        };
        let resp = self
            .inner
            .get(
                endpoint,
                ctx.user.id.as_str(),
                &[("uri", &path)],
                HTTP_TIMEOUT,
            )
            .await
            .map_err(|e| aura_tools::ToolError::Execution(e.to_string()))?;
        let result = resp.get("result").cloned().unwrap_or(resp);

        if action == "list" || action == "tree" {
            let raw_entries = if result.is_array() {
                Some(result.clone())
            } else if let Some(obj) = result.as_object() {
                obj.get("entries")
                    .or_else(|| obj.get("items"))
                    .or_else(|| obj.get("children"))
                    .cloned()
            } else {
                None
            };
            if let Some(Value::Array(arr)) = raw_entries {
                let entries: Vec<Value> = arr
                    .into_iter()
                    .take(50)
                    .map(|e| {
                        let uri = e.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                        let name = e
                            .get("rel_path")
                            .or_else(|| e.get("name"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| {
                                uri.rsplit_once('/')
                                    .map(|(_, n)| n.to_string())
                                    .unwrap_or_default()
                            });
                        let is_dir = e.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false)
                            || e.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false)
                            || e.get("type").and_then(|v| v.as_str()) == Some("dir");
                        json!({
                            "name": name,
                            "uri": uri,
                            "type": if is_dir { "dir" } else { "file" },
                            "abstract": e.get("abstract").and_then(|v| v.as_str()).unwrap_or(""),
                        })
                    })
                    .collect();
                return Ok(ToolOutput::Json(json!({"path": path, "entries": entries})));
            }
        }
        Ok(ToolOutput::Json(result))
    }
}

struct VikingRememberTool {
    inner: Arc<OpenVikingInner>,
}

#[async_trait]
impl Tool for VikingRememberTool {
    fn name(&self) -> &str {
        TOOL_REMEMBER
    }
    fn description(&self) -> String {
        "Explicitly store a fact or memory in the OpenViking knowledge base. Use for \
         important information the agent should remember long-term. The system \
         automatically categorizes and indexes the memory."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "The information to remember."},
                "category": {
                    "type": "string",
                    "enum": ["preference", "entity", "event", "case", "pattern"],
                    "description": "Memory category (default: auto-detected)."
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| aura_tools::ToolError::InvalidParams("content is required".into()))?;
        if content.is_empty() {
            return Err(aura_tools::ToolError::InvalidParams(
                "content cannot be empty".into(),
            ));
        }
        let category = params
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let subdir = category_to_subdir(category);
        let uri = build_memory_uri(ctx.user.id.as_str(), subdir);
        let body = json!({"uri": uri, "content": content, "mode": "create"});
        let resp = self
            .inner
            .post_json(
                "/api/v1/content/write",
                ctx.user.id.as_str(),
                &body,
                HTTP_TIMEOUT,
            )
            .await
            .map_err(|e| aura_tools::ToolError::Execution(e.to_string()))?;
        let written = resp
            .get("result")
            .and_then(|r| r.get("written_bytes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(ToolOutput::Json(json!({
            "status": "stored",
            "uri": uri,
            "message": format!("Memory stored ({written}b) and queued for vector indexing."),
        })))
    }
}

struct VikingAddResourceTool {
    inner: Arc<OpenVikingInner>,
}

#[async_trait]
impl Tool for VikingAddResourceTool {
    fn name(&self) -> &str {
        TOOL_ADD_RESOURCE
    }
    fn description(&self) -> String {
        "Add a remote URL or local file/directory to the OpenViking knowledge base. \
         Remote resources must be public http(s), git, or ssh URLs. Local files and \
         directories are uploaded via temp_upload; directories are zipped first."
            .into()
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Remote URL or local file/directory path to add."},
                "reason": {"type": "string", "description": "Why this resource is relevant (improves search)."},
                "to": {"type": "string", "description": "Optional target viking:// URI for the resource."},
                "parent": {"type": "string", "description": "Optional parent viking:// URI. Cannot be used with `to`."},
                "instruction": {"type": "string", "description": "Optional processing instruction for semantic extraction."},
                "wait": {"type": "boolean", "description": "Whether to wait for processing to complete."},
                "timeout": {"type": "number", "description": "Timeout in seconds when `wait` is true."}
            },
            "required": ["url"]
        })
    }

    /// Declare ReadFile access for any local URL — the executor's approval
    /// gate skips ReadFile in this codebase, so this is more about the
    /// governance contract than per-call prompting. The sensitive-path +
    /// size + SSRF gates inside `execute` are the load-bearing checks.
    fn accessed_resources(&self, params: &Value) -> Vec<aura_model::ResourceAccess> {
        let Some(url) = params.get("url").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        match classify_resource(url) {
            ResourceSource::Local(p) => vec![aura_model::ResourceAccess::ReadFile { path: p }],
            ResourceSource::Remote => Vec::new(),
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| aura_tools::ToolError::InvalidParams("url is required".into()))?
            .to_string();
        if params.get("to").is_some() && params.get("parent").is_some() {
            return Err(aura_tools::ToolError::InvalidParams(
                "Cannot specify both 'to' and 'parent'".into(),
            ));
        }

        let mut payload = serde_json::Map::new();
        for key in ["reason", "to", "parent", "instruction", "wait", "timeout"] {
            if let Some(v) = params.get(key)
                && !v.is_null()
                && v.as_str() != Some("")
            {
                payload.insert(key.into(), v.clone());
            }
        }

        let user_id = ctx.user.id.as_str();
        let source = classify_resource(&url);
        let mut cleanup: Option<PathBuf> = None;
        let result = match source {
            ResourceSource::Remote => {
                // The model can hand us an arbitrary http(s) URL that the
                // OpenViking server will fetch on our behalf — without this
                // guard the server becomes an SSRF proxy onto loopback /
                // RFC1918 / link-local. Mirrors WebFetch's `validate_url_with`
                // for the http(s) subset; git/ssh URLs are allowed through
                // since they are not arbitrary HTTP and the credential model
                // is upstream.
                reject_private_remote_url(&url)?;
                payload.insert("path".into(), json!(url));
                self.inner
                    .post_json(
                        "/api/v1/resources",
                        user_id,
                        &Value::Object(payload),
                        HTTP_TIMEOUT,
                    )
                    .await
            }
            ResourceSource::Local(path) => {
                // Sensitive-path guard mirrors `send_local_file`: this is the
                // only other tool that ships local bytes to a remote endpoint,
                // and the executor's approval gate skips ReadFile entirely
                // (see `tool_executor` / `feedback_read_no_confirm`), so
                // `is_sensitive_path` is the load-bearing check.
                if aura_security::is_sensitive_path(&path) {
                    return Err(aura_tools::ToolError::Execution(format!(
                        "Refusing to upload sensitive path: {url}"
                    )));
                }
                if !path.exists() {
                    return Err(aura_tools::ToolError::Execution(format!(
                        "Local resource path does not exist: {url}"
                    )));
                }
                let upload_path = if path.is_dir() {
                    payload.insert(
                        "source_name".into(),
                        json!(
                            path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("upload")
                        ),
                    );
                    let dir = path.clone();
                    // WalkDir + read + deflate is fully synchronous; running
                    // it on the tokio worker would stall other tasks for big
                    // trees. spawn_blocking releases the worker.
                    let zip = tokio::task::spawn_blocking(move || zip_directory(&dir))
                        .await
                        .map_err(|e| {
                            aura_tools::ToolError::Execution(format!("zip join failed: {e}"))
                        })?
                        .map_err(|e| {
                            aura_tools::ToolError::Execution(format!("zip directory failed: {e}"))
                        })?;
                    cleanup = Some(zip.clone());
                    zip
                } else if path.is_file() {
                    payload.insert(
                        "source_name".into(),
                        json!(
                            path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("upload.bin")
                        ),
                    );
                    path.clone()
                } else {
                    return Err(aura_tools::ToolError::Execution(format!(
                        "Unsupported local resource path: {url}"
                    )));
                };
                // Size cap (matches `send_local_file::MAX_BYTES`). Applies
                // to the zipped archive too, so a model can't point at /
                // and force a multi-gig upload.
                let size = tokio::fs::metadata(&upload_path)
                    .await
                    .map_err(|e| {
                        aura_tools::ToolError::Execution(format!("stat upload path: {e}"))
                    })?
                    .len();
                if size > MAX_UPLOAD_BYTES {
                    let _ = cleanup.take().map(|p| std::fs::remove_file(p).ok());
                    return Err(aura_tools::ToolError::Execution(format!(
                        "{url} is {size} bytes, exceeds the {MAX_UPLOAD_BYTES}-byte cap"
                    )));
                }
                let temp_file_id = self
                    .inner
                    .upload_temp_file(user_id, &upload_path)
                    .await
                    .map_err(|e| aura_tools::ToolError::Execution(e.to_string()))?;
                payload.insert("temp_file_id".into(), json!(temp_file_id));
                self.inner
                    .post_json(
                        "/api/v1/resources",
                        user_id,
                        &Value::Object(payload),
                        HTTP_TIMEOUT,
                    )
                    .await
            }
        };

        if let Some(p) = cleanup
            && let Err(e) = std::fs::remove_file(&p)
        {
            debug!(path = %p.display(), error = %e, "openviking add_resource: zip cleanup failed");
        }

        let resp = result.map_err(|e| aura_tools::ToolError::Execution(e.to_string()))?;
        let root_uri = resp
            .get("result")
            .and_then(|r| r.get("root_uri"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(ToolOutput::Json(json!({
            "status": "added",
            "root_uri": root_uri,
            "message": "Resource queued for processing. Use viking_search after a moment to find it.",
        })))
    }
}

/// Reject http(s) URLs whose host is `localhost` (or `*.localhost`) or whose
/// literal IP falls into a blocked SSRF range (loopback / RFC1918 /
/// link-local / unspecified / CGNAT / multicast / reserved). Hostname-target
/// SSRF on a non-loopback name is left to the OpenViking server's network
/// boundary — same trade-off as WebFetch's URL pre-check.
fn reject_private_remote_url(url: &str) -> aura_tools::Result<()> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Ok(());
    }
    let parsed = url::Url::parse(url)
        .map_err(|e| aura_tools::ToolError::InvalidParams(format!("invalid url `{url}`: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| aura_tools::ToolError::InvalidParams("url has no host".into()))?;
    let host_lc = host.to_ascii_lowercase();
    if host_lc == "localhost"
        || host_lc == "localhost.localdomain"
        || host_lc.ends_with(".localhost")
    {
        return Err(aura_tools::ToolError::Execution(format!(
            "host `{host}` blocked (loopback-like name)"
        )));
    }
    let stripped = host_lc
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&host_lc);
    if let Ok(addr) = stripped.parse::<IpAddr>()
        && aura_security::is_blocked_ip(&addr, false)
    {
        return Err(aura_tools::ToolError::Execution(format!(
            "ip `{addr}` blocked"
        )));
    }
    Ok(())
}

enum ResourceSource {
    Remote,
    Local(PathBuf),
}

fn classify_resource(value: &str) -> ResourceSource {
    if REMOTE_PREFIXES.iter().any(|p| value.starts_with(p)) {
        return ResourceSource::Remote;
    }
    if let Some(rest) = value.strip_prefix("file://") {
        return ResourceSource::Local(PathBuf::from(rest));
    }
    if value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
    {
        let expanded = if let Some(rest) = value.strip_prefix("~/")
            && let Some(home) = std::env::var_os("HOME")
        {
            let mut p = PathBuf::from(home);
            p.push(rest);
            p
        } else {
            PathBuf::from(value)
        };
        return ResourceSource::Local(expanded);
    }
    if value.contains('/') {
        return ResourceSource::Local(PathBuf::from(value));
    }
    ResourceSource::Remote
}

fn zip_directory(dir: &Path) -> std::io::Result<PathBuf> {
    let tmp = std::env::temp_dir();
    let zip_name = format!("openviking_upload_{}.zip", Uuid::new_v4().simple());
    let zip_path = tmp.join(zip_name);
    let file = File::create(&zip_path)?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let dir = dir.to_path_buf();
    for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_symlink() || path.is_dir() {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let arcname = path
            .strip_prefix(&dir)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            });
        writer.start_file(arcname, options)?;
        let bytes = std::fs::read(path)?;
        writer.write_all(&bytes)?;
    }
    writer.finish()?;
    Ok(zip_path)
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
        assert_eq!(cfg.agent(), DEFAULT_AGENT);
        assert_eq!(cfg.top_k(), DEFAULT_TOP_K);
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
    fn normalize_summary_uri_strips_pseudo_suffixes() {
        assert_eq!(
            normalize_summary_uri("viking://user/x/.abstract.md"),
            "viking://user/x"
        );
        assert_eq!(
            normalize_summary_uri("viking://user/x/.overview.md"),
            "viking://user/x"
        );
        assert_eq!(
            normalize_summary_uri("viking://user/x"),
            "viking://user/x",
            "non-pseudo URI unchanged"
        );
    }

    #[test]
    fn category_to_subdir_maps_or_defaults() {
        assert_eq!(category_to_subdir("preference"), "preferences");
        assert_eq!(category_to_subdir("entity"), "entities");
        assert_eq!(category_to_subdir("unknown"), DEFAULT_MEMORY_SUBDIR);
        assert_eq!(category_to_subdir(""), DEFAULT_MEMORY_SUBDIR);
    }

    #[test]
    fn classify_resource_distinguishes_remote_and_local() {
        assert!(matches!(
            classify_resource("https://example.com/x"),
            ResourceSource::Remote
        ));
        assert!(matches!(
            classify_resource("git@github.com:user/repo.git"),
            ResourceSource::Remote
        ));
        assert!(matches!(
            classify_resource("/abs/path"),
            ResourceSource::Local(_)
        ));
        assert!(matches!(
            classify_resource("./rel/path"),
            ResourceSource::Local(_)
        ));
    }

    #[test]
    fn build_memory_uri_includes_user_and_subdir() {
        let uri = build_memory_uri("booiris", "preferences");
        assert!(uri.starts_with("viking://user/booiris/memories/preferences/mem_"));
        assert!(uri.ends_with(".md"));
    }

    #[test]
    fn zip_directory_produces_readable_archive() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub").join("b.txt"), b"world").unwrap();
        let zip = zip_directory(tmp.path()).unwrap();
        let f = File::open(&zip).unwrap();
        let archive = zip::ZipArchive::new(f).unwrap();
        let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.iter().any(|n| n.ends_with("b.txt")));
        std::fs::remove_file(&zip).ok();
    }

    #[test]
    fn truncate_str_respects_utf8_boundaries() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
        let s = "héllo";
        assert!(truncate_str(s, 3).len() <= 3);
    }
}
