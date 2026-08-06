use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use baybo_security::SecretVault;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::Tool as RmcpTool;
use rmcp::service::{Peer, RoleClient, RunningService, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::auth::{AuthClient, AuthorizationManager, CredentialStore, OAuthClientConfig};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

use crate::mcp::config::{McpServerEntry, McpTransportConfig};
use crate::mcp::credentials::VaultCredentialStore;
use crate::mcp::error::{McpError, McpResult};
use crate::mcp::log_line::SidecarLogLine;
use crate::mcp::vault_keys;

// The browser sidecar owns a 25-second Docker cleanup deadline.
const STDIO_CHILD_SHUTDOWN_GRACE: Duration = Duration::from_secs(27);

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
}

pub async fn connect(
    entry: &McpServerEntry,
    vault: &Arc<SecretVault>,
    process_manager: &Arc<baybo_process::ProcessManager>,
    proxy: Option<&baybo_security::http::ProxySettings>,
) -> McpResult<McpServerSession> {
    connect_with_extra_env(entry, vault, process_manager, &HashMap::new(), proxy).await
}

/// Build a `reqwest::Client` for an HTTP MCP transport, honoring the optional
/// egress proxy.
fn proxied_http_client(
    proxy: Option<&baybo_security::http::ProxySettings>,
) -> McpResult<reqwest::Client> {
    baybo_security::http::client(proxy)
        .map_err(|e| McpError::Transport(format!("build proxied http client: {e}")))
}

/// Like [`connect`] but merges `extra_env` onto the stdio child's env
/// after the vault load. Used by the reconciler for embedded servers
/// to inject boot-config env vars (`BAYBO_BROWSER_PROFILE_DIR`, …)
/// without polluting the user's secret vault. Vault entries retain
/// precedence — `extra_env` is applied first, then vault entries, so
/// an operator who really wants to override a boot value can stash
/// it under `mcp.<name>.env`.
pub async fn connect_with_extra_env(
    entry: &McpServerEntry,
    vault: &Arc<SecretVault>,
    process_manager: &Arc<baybo_process::ProcessManager>,
    extra_env: &HashMap<String, String>,
    proxy: Option<&baybo_security::http::ProxySettings>,
) -> McpResult<McpServerSession> {
    let running = match &entry.transport {
        McpTransportConfig::Stdio { command, args } => {
            connect_stdio(
                &entry.name,
                command,
                args,
                vault,
                process_manager,
                extra_env,
                proxy,
            )
            .await?
        }
        McpTransportConfig::Http { url } => connect_http(&entry.name, url, vault, proxy).await?,
    };

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
    process_manager: &Arc<baybo_process::ProcessManager>,
    extra_env: &HashMap<String, String>,
    proxy: Option<&baybo_security::http::ProxySettings>,
) -> McpResult<RunningService<RoleClient, ()>> {
    let env = load_string_map(vault, &vault_keys::env_bag(server_name)).await?;

    let mut tokio_cmd = Command::new(command);
    tokio_cmd.args(args).env_clear();
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
    // Egress proxy (if configured). The env_clear above wiped any inherited
    // *_PROXY vars, so inject the standard ones explicitly; loopback stays
    // direct (see `ProxySettings::env_vars`) so a local-CDP browser server
    // isn't routed through the proxy. extra_env / vault still override below.
    if let Some(proxy) = proxy {
        for (k, v) in proxy.env_vars() {
            tokio_cmd.env(k, v);
        }
    }
    // Embedded boot-config env first; vault overrides if both present.
    for (k, v) in extra_env {
        tokio_cmd.env(k, v);
    }
    for (k, v) in env {
        tokio_cmd.env(k, v);
    }

    tokio_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process_manager
        .spawn(&mut tokio_cmd, format!("mcp:{server_name}"))
        .map_err(|e| McpError::Transport(format!("spawn '{command}': {e}")))?;
    let stdin = child
        .take_stdin()
        .ok_or_else(|| McpError::Transport("mcp child stdin pipe missing".into()))?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| McpError::Transport("mcp child stdout pipe missing".into()))?;
    let stderr = child.take_stderr();
    if let Some(stderr) = stderr {
        drain_mcp_stderr(server_name.to_owned(), stderr);
    }

    ().serve(ManagedChildTransport::new(child, stdout, stdin))
        .await
        .map_err(|e| McpError::Connection(e.to_string()))
}

struct ManagedChildTransport {
    child: baybo_process::ManagedChild,
    transport: AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>,
}

impl ManagedChildTransport {
    fn new(child: baybo_process::ManagedChild, stdout: ChildStdout, stdin: ChildStdin) -> Self {
        Self {
            child,
            transport: AsyncRwTransport::new_client(stdout, stdin),
        }
    }
}

impl Transport<RoleClient> for ManagedChildTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let transport_result = self.transport.close().await;
        let child_result = self
            .child
            .shutdown(STDIO_CHILD_SHUTDOWN_GRACE)
            .await
            .map(|_| ());
        transport_result.and(child_result)
    }
}

/// Per-spawn budget on how many stderr lines a stdio MCP server may emit
/// at their routed level before the drain demotes the remainder to
/// `debug!`. First-party sidecars normally write a handful of NDJSON
/// lines per boot, but one can shell out to a chatty subprocess — a
/// `docker build` transcript is thousands of `apt`/layer lines — and an
/// operator-added third-party server has no such contract at all. The
/// budget keeps a single spawn's output from burying `baybo.log`; the
/// overflow is still reachable at `RUST_LOG=debug`.
const MCP_STDERR_INFO_LINE_BUDGET: usize = 300;

/// Routing level recovered from a sidecar NDJSON line (or the plain-text
/// default). Kept as an enum so emission goes through distinct `tracing`
/// callsites rather than a runtime-`Level` dispatch.
enum StderrLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Map a sidecar NDJSON `level` string onto a routing level, mirroring
/// the aliases the gateway's `LogLevel::parse` accepts. `info` and any
/// unrecognized label route to `Info` — the safe default that preserves
/// today's behavior for third-party servers whose stderr isn't our
/// NDJSON shape.
fn routed_stderr_level(level: &str) -> StderrLevel {
    match level.to_ascii_lowercase().as_str() {
        "error" | "err" => StderrLevel::Error,
        "warn" | "warning" => StderrLevel::Warn,
        "debug" | "trace" => StderrLevel::Debug,
        _ => StderrLevel::Info,
    }
}

/// Forward a stdio MCP server's stderr into the tracing subscriber.
///
/// rmcp inherits the child's stderr by default; [`connect_stdio`] spawns
/// it `piped` and hands the stream here. Each line becomes a `tracing`
/// event tagged with `mcp_server`, so the gateway's file subscriber
/// records server diagnostics in `baybo.log` (and the admin LogBuffer)
/// rather than scrolling them past on the terminal. The task ends on EOF
/// when the child exits.
///
/// First-party sidecars log NDJSON (`{"level","msg","target"?}`, the
/// shared [`SidecarLogLine`] shape); each line is re-emitted at its
/// declared level so boot-progress chatter demotes to `debug!` while
/// warnings and errors stay loud. A line that isn't that shape — every
/// third-party MCP server's plain-text stderr — is passed through
/// unchanged at `info!`. A per-spawn [`MCP_STDERR_INFO_LINE_BUDGET`]
/// caps the routed-level output: past the budget every remaining line
/// drops to `debug!` (one `warn!` marks the trip so the truncation is
/// visible), which keeps a runaway subprocess transcript from flooding
/// the log at default verbosity.
fn drain_mcp_stderr(server_name: String, stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut lines_seen: usize = 0;
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    lines_seen += 1;
                    let (routed, msg) = match serde_json::from_str::<SidecarLogLine>(&line) {
                        Ok(parsed) => (routed_stderr_level(&parsed.level), parsed.msg),
                        Err(_) => (StderrLevel::Info, line),
                    };
                    if lines_seen == MCP_STDERR_INFO_LINE_BUDGET + 1 {
                        tracing::warn!(
                            mcp_server = %server_name,
                            budget = MCP_STDERR_INFO_LINE_BUDGET,
                            "mcp server stderr exceeded line budget this spawn; further output demoted to debug",
                        );
                    }
                    let effective = if lines_seen > MCP_STDERR_INFO_LINE_BUDGET {
                        StderrLevel::Debug
                    } else {
                        routed
                    };
                    match effective {
                        StderrLevel::Debug => tracing::debug!(mcp_server = %server_name, "{msg}"),
                        StderrLevel::Info => tracing::info!(mcp_server = %server_name, "{msg}"),
                        StderrLevel::Warn => tracing::warn!(mcp_server = %server_name, "{msg}"),
                        StderrLevel::Error => tracing::error!(mcp_server = %server_name, "{msg}"),
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(
                        mcp_server = %server_name,
                        error = %e,
                        "mcp server stderr read failed",
                    );
                    break;
                }
            }
        }
    });
}

async fn connect_http(
    server_name: &str,
    url: &str,
    vault: &Arc<SecretVault>,
    proxy: Option<&baybo_security::http::ProxySettings>,
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
        // and then every request 401s until the operator re-runs `baybo
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

        let auth_client = AuthClient::new(proxied_http_client(proxy)?, manager);
        let transport = StreamableHttpClientTransport::with_client(auth_client, config);
        ().serve(transport)
            .await
            .map_err(|e| McpError::Connection(e.to_string()))
    } else {
        // No OAuth: plain reqwest client. Static-bearer / API-key servers
        // get their auth via the `--header` bag, which is already in
        // `config.custom_headers`.
        let client = proxied_http_client(proxy)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_level_mirrors_loglevel_aliases() {
        assert!(matches!(routed_stderr_level("error"), StderrLevel::Error));
        assert!(matches!(routed_stderr_level("ERR"), StderrLevel::Error));
        assert!(matches!(routed_stderr_level("warn"), StderrLevel::Warn));
        assert!(matches!(routed_stderr_level("Warning"), StderrLevel::Warn));
        assert!(matches!(routed_stderr_level("debug"), StderrLevel::Debug));
        assert!(matches!(routed_stderr_level("trace"), StderrLevel::Debug));
        assert!(matches!(routed_stderr_level("info"), StderrLevel::Info));
    }

    #[test]
    fn unknown_level_falls_back_to_info() {
        assert!(matches!(routed_stderr_level(""), StderrLevel::Info));
        assert!(matches!(routed_stderr_level("verbose"), StderrLevel::Info));
    }

    #[test]
    fn ndjson_line_yields_declared_level_and_msg() {
        let parsed: SidecarLogLine =
            serde_json::from_str(r#"{"level":"debug","msg":"[browser-mcp] image cached"}"#)
                .expect("valid NDJSON parses");
        assert!(matches!(
            routed_stderr_level(&parsed.level),
            StderrLevel::Debug
        ));
        assert_eq!(parsed.msg, "[browser-mcp] image cached");
    }

    #[test]
    fn plain_text_line_is_not_ndjson() {
        // The third-party passthrough path: a non-JSON stderr line does
        // not parse, so the drain keeps it at info! verbatim.
        assert!(serde_json::from_str::<SidecarLogLine>("Listening on stdio").is_err());
        // A JSON value of the wrong shape (no level/msg) also falls back.
        assert!(serde_json::from_str::<SidecarLogLine>(r#"{"foo":1}"#).is_err());
    }
}
