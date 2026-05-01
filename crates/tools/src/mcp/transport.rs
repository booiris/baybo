use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use aura_security::SecretVault;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{
    Annotated, CallToolRequestParams, Meta, RawContent, Tool as RmcpTool,
};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::auth::{AuthClient, AuthorizationManager, CredentialStore, OAuthClientConfig};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::ToolOutput;
use crate::mcp::config::{McpServerEntry, McpTransportConfig};
use crate::mcp::credentials::VaultCredentialStore;
use crate::mcp::error::{McpError, McpResult};
use crate::mcp::sidecar::{SidecarSender, SidecarTransport};
use crate::mcp::vault_keys;

pub struct McpServerSession {
    running: RunningService<RoleClient, ()>,
    tools: Vec<RmcpTool>,
}

impl McpServerSession {
    pub fn peer(&self) -> Peer<RoleClient> {
        self.running.peer().clone()
    }

    pub fn tools(&self) -> &[RmcpTool] {
        &self.tools
    }

    pub async fn shutdown(self) {
        let _ = self.running.cancel().await;
    }

    /// Call a tool on the connected MCP server.
    ///
    /// `aura_session_id`, when `Some(id)`, is forwarded through
    /// `_meta.auraSessionId` so a sidecar that hosts a multi-bot
    /// MCP server can map the call to the right tenant. The
    /// session id semantics match `aura_model::Session::id`.
    ///
    /// `params` is the LLM-supplied JSON. An object becomes the
    /// rmcp `arguments`; null means "no arguments". Anything else
    /// is rejected with a typed error string the caller can
    /// surface as the LLM tool result.
    pub async fn call_tool(
        &self,
        name: &str,
        params: Value,
        aura_session_id: Option<&str>,
    ) -> Result<ToolOutput, String> {
        call_tool_via_peer(&self.peer(), name, params, aura_session_id).await
    }
}

/// Invoke a tool on an already-connected rmcp peer.
///
/// Split out from [`McpServerSession::call_tool`] so callers that
/// hold only a `Peer<RoleClient>` (e.g. the gateway's
/// `SidecarMcpManager`, which keeps the sidecar's session inside a
/// channel-keyed cache) can dispatch without exposing the rmcp
/// types into their own crate.
pub async fn call_tool_via_peer(
    peer: &Peer<RoleClient>,
    name: &str,
    params: Value,
    aura_session_id: Option<&str>,
) -> Result<ToolOutput, String> {
    let arguments = match params {
        Value::Object(map) => Some(map),
        Value::Null => None,
        other => {
            return Err(format!(
                "MCP tool arguments must be a JSON object; got {}",
                type_name(&other)
            ));
        }
    };
    let mut request = CallToolRequestParams::new(name.to_string());
    if let Some(args) = arguments {
        request = request.with_arguments(args);
    }
    if let Some(id) = aura_session_id {
        let mut meta = Meta::new();
        meta.0
            .insert("auraSessionId".into(), Value::String(id.to_string()));
        request.meta = Some(meta);
    }
    let result = peer
        .call_tool(request)
        .await
        .map_err(|e| format!("call_tool {name}: {e}"))?;
    let text = format_call_result_content(&result.content);
    if result.is_error.unwrap_or(false) {
        Ok(ToolOutput::Error(text))
    } else {
        Ok(ToolOutput::Text(text))
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn format_call_result_content(parts: &[Annotated<RawContent>]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match &part.raw {
            RawContent::Text(t) => out.push_str(&t.text),
            RawContent::Image(_) => out.push_str("[image content elided]"),
            RawContent::Audio(_) => out.push_str("[audio content elided]"),
            RawContent::Resource(_) => out.push_str("[resource content elided]"),
            RawContent::ResourceLink(_) => out.push_str("[resource link elided]"),
        }
    }
    out
}

pub async fn connect(
    entry: &McpServerEntry,
    vault: &Arc<SecretVault>,
) -> McpResult<McpServerSession> {
    let running = match &entry.transport {
        McpTransportConfig::Stdio { command, args } => {
            connect_stdio(&entry.name, command, args, vault).await?
        }
        McpTransportConfig::Http { url } => connect_http(&entry.name, url, vault).await?,
    };

    finalize_session(running).await
}

/// Run the rmcp handshake against a sidecar-hosted MCP server.
///
/// Callers provide the byte-pipe halves directly — a cloneable
/// outbound [`SidecarSender`] plus an inbound `mpsc::Receiver`. This
/// keeps the whole rmcp surface inside `aura-tools` so callers (e.g.
/// the gateway's channel layer) don't need an rmcp dependency to
/// stand up a session.
///
/// On success: the returned [`McpServerSession`] holds the running
/// rmcp service and a snapshot of tools advertised at handshake
/// time. Drop it via `shutdown()` to tear the session down cleanly.
pub async fn connect_sidecar(
    sender: Arc<dyn SidecarSender>,
    inbound: mpsc::Receiver<Vec<u8>>,
) -> McpResult<McpServerSession> {
    let transport = SidecarTransport::new(sender, inbound);
    let running = ()
        .serve(transport)
        .await
        .map_err(|e| McpError::Connection(e.to_string()))?;
    finalize_session(running).await
}

async fn finalize_session(
    running: RunningService<RoleClient, ()>,
) -> McpResult<McpServerSession> {
    let tools = running
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| McpError::Protocol(format!("list_all_tools: {e}")))?;
    Ok(McpServerSession { running, tools })
}

async fn connect_stdio(
    server_name: &str,
    command: &str,
    args: &[String],
    vault: &Arc<SecretVault>,
) -> McpResult<RunningService<RoleClient, ()>> {
    let env = load_string_map(vault, &vault_keys::env_bag(server_name)).await?;

    let mut tokio_cmd = Command::new(command);
    tokio_cmd
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for var in ["PATH", "HOME", "LANG", "TZ", "TMPDIR"] {
        if let Ok(value) = std::env::var(var) {
            tokio_cmd.env(var, value);
        }
    }
    for (k, v) in std::env::vars() {
        if k.starts_with("LC_") {
            tokio_cmd.env(k, v);
        }
    }
    for (k, v) in env {
        tokio_cmd.env(k, v);
    }

    let (process, _stderr) = TokioChildProcess::builder(tokio_cmd)
        .spawn()
        .map_err(|e| McpError::Transport(format!("spawn '{command}': {e}")))?;

    ().serve(process)
        .await
        .map_err(|e| McpError::Connection(e.to_string()))
}

async fn connect_http(
    server_name: &str,
    url: &str,
    vault: &Arc<SecretVault>,
) -> McpResult<RunningService<RoleClient, ()>> {
    let headers = load_string_map(vault, &vault_keys::header_bag(server_name)).await?;

    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if !headers.is_empty() {
        let mut as_hashmap: HashMap<HeaderName, HeaderValue> = HashMap::new();
        for (name, value) in headers {
            let header_name: HeaderName = name
                .parse()
                .map_err(|e| McpError::Transport(format!("invalid header name '{name}': {e}")))?;
            let header_value: HeaderValue = value
                .parse()
                .map_err(|e| McpError::Transport(format!("invalid header value: {e}")))?;
            as_hashmap.insert(header_name, header_value);
        }
        config = config.custom_headers(as_hashmap);
    }

    let creds_store = VaultCredentialStore::new(Arc::clone(vault), server_name);
    let has_creds = creds_store
        .load()
        .await
        .map_err(|e| McpError::OAuth(format!("read stored credentials: {e}")))?
        .is_some();

    if has_creds {
        // OAuth path: AuthClient injects the bearer token per-request and
        // refreshes transparently when the cached access token expires —
        // the rotated tokens land back in the vault via VaultCredentialStore.
        let mut manager = AuthorizationManager::new(url)
            .await
            .map_err(|e| McpError::OAuth(format!("oauth manager init: {e}")))?;
        manager.set_credential_store(VaultCredentialStore::new(Arc::clone(vault), server_name));
        manager
            .initialize_from_store()
            .await
            .map_err(|e| McpError::OAuth(format!("load credentials from vault: {e}")))?;

        // For confidential clients, `initialize_from_store` only sets the
        // client_id (StoredCredentials does not carry the secret). Re-attach
        // the secret from the vault so the refresh-token grant authenticates
        // — without this, the access token rotates fine until first expiry
        // and then every request 401s until the operator re-runs `aura
        // mcp add`.
        if let Some(secret) = vault
            .get_secret(&vault_keys::oauth_client_secret(server_name))
            .await
            .map_err(|e| McpError::OAuth(format!("read oauth client secret: {e}")))?
        {
            let secret = String::from_utf8(secret.as_bytes().to_vec())
                .map_err(|e| McpError::OAuth(format!("oauth client secret is not utf-8: {e}")))?;
            let (client_id, _) = manager
                .get_credentials()
                .await
                .map_err(|e| McpError::OAuth(format!("read client id from store: {e}")))?;
            // The redirect_uri is required by the oauth2 builder but is
            // never used during a refresh-token grant — the placeholder
            // here matches what rmcp uses for non-redirect flows.
            let client_config =
                OAuthClientConfig::new(&client_id, "http://localhost").with_client_secret(secret);
            manager
                .configure_client(client_config)
                .map_err(|e| McpError::OAuth(format!("attach client secret for refresh: {e}")))?;
        }

        let auth_client = AuthClient::new(reqwest::Client::new(), manager);
        let transport = StreamableHttpClientTransport::with_client(auth_client, config);
        ().serve(transport)
            .await
            .map_err(|e| McpError::Connection(e.to_string()))
    } else {
        // No OAuth: plain reqwest client. Static-bearer / API-key servers
        // get their auth via the `--header` bag, which is already in
        // `config.custom_headers`.
        let client = reqwest::Client::new();
        let transport = StreamableHttpClientTransport::with_client(client, config);
        ().serve(transport)
            .await
            .map_err(|e| McpError::Connection(e.to_string()))
    }
}

async fn load_string_map(vault: &Arc<SecretVault>, key: &str) -> McpResult<Vec<(String, String)>> {
    let secret = vault
        .get_secret(key)
        .await
        .map_err(|e| McpError::OAuth(format!("read vault key '{key}': {e}")))?;
    let Some(secret) = secret else {
        return Ok(Vec::new());
    };
    let value: Value = serde_json::from_slice(secret.as_bytes())
        .map_err(|e| McpError::OAuth(format!("decode vault key '{key}': {e}")))?;
    let map = value.as_object().ok_or_else(|| {
        McpError::OAuth(format!("vault key '{key}' did not contain a JSON object"))
    })?;
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        let s = v
            .as_str()
            .ok_or_else(|| McpError::OAuth(format!("vault key '{key}.{k}' is not a string")))?;
        out.push((k.clone(), s.to_string()));
    }
    Ok(out)
}
