//! Shared connection probe used by `aura mcp list` and `aura mcp get`.
//!
//! `connect()` against an MCP server is heavier than a typical CLI read —
//! stdio servers spawn a subprocess, HTTP servers run the rmcp handshake.
//! `aura mcp` commands accept a `--no-probe` flag for scripting paths that
//! want config-only output.

use std::sync::Arc;
use std::time::Duration;

use aura_security::SecretVault;
use aura_tools::mcp::transport::connect;
use aura_tools::mcp::{McpServerEntry, McpTransportConfig, vault_keys};

const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const HTTP_PRECHECK_TIMEOUT: Duration = Duration::from_secs(12);
const HTTP_PRECHECK_ATTEMPTS: usize = 3;

pub enum ProbeStatus {
    Skipped,
    Ok { tool_count: usize },
    Timeout,
    AuthRequired(String),
    Unreachable(String),
    Error(String),
}

impl ProbeStatus {
    pub fn short(&self) -> String {
        match self {
            Self::Skipped => "(probe skipped)".into(),
            Self::Ok { tool_count } => format!("ok ({tool_count} tools)"),
            Self::Timeout => "timeout".into(),
            Self::AuthRequired(_) => "auth required (run `aura mcp add` to re-authorize)".into(),
            Self::Unreachable(msg) => format!("unreachable: {}", clip(msg, 60)),
            Self::Error(msg) => format!("error: {}", clip(msg, 60)),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Ok { .. } => "ok",
            Self::Timeout => "timeout",
            Self::AuthRequired(_) => "auth_required",
            Self::Unreachable(_) => "unreachable",
            Self::Error(_) => "error",
        }
    }

    pub fn tool_count(&self) -> Option<usize> {
        match self {
            Self::Ok { tool_count } => Some(*tool_count),
            _ => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::AuthRequired(m) | Self::Unreachable(m) | Self::Error(m) => Some(m.as_str()),
            _ => None,
        }
    }
}

pub async fn probe(entry: &McpServerEntry, vault: &Arc<SecretVault>) -> ProbeStatus {
    // Fast path: for an HTTP server with no stored access token, a single
    // unauthenticated GET tells us whether the server requires auth far
    // more reliably than letting rmcp swallow the response into an opaque
    // "TransportClosed" error. Stdio servers go straight to rmcp because
    // there is no equivalent cheap probe.
    if let McpTransportConfig::Http { url } = &entry.transport
        && !has_access_token(vault, &entry.name).await
        && let Some(status) = http_auth_precheck(url).await
    {
        return status;
    }

    match tokio::time::timeout(PROBE_TIMEOUT, connect(entry, vault)).await {
        Err(_) => ProbeStatus::Timeout,
        Ok(Err(e)) => classify(e.to_string()),
        Ok(Ok(session)) => {
            let count = session.tools().len();
            session.shutdown().await;
            ProbeStatus::Ok { tool_count: count }
        }
    }
}

async fn has_access_token(vault: &Arc<SecretVault>, server_name: &str) -> bool {
    vault
        .get_secret(&vault_keys::oauth_credentials(server_name))
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Returns `Some` if a fast unauthenticated probe of the URL gives us a
/// definitive answer (auth required, or unreachable). Returns `None` if
/// the response is a 2xx (server is reachable and may be open) or any
/// status that doesn't tell us anything useful — in those cases the
/// caller falls through to the full rmcp handshake.
///
/// Strategy: POST a minimal JSON-RPC `initialize` request — the same
/// first thing an MCP client sends. This is the most accurate signal
/// because we drive exactly the path the server is built to handle.
/// HEAD/GET are unreliable: many MCP servers block them entirely
/// (figma's endpoint times out on HEAD; github's truncates on GET).
///
/// We retry once on connect failures because the first hit against a
/// cold host occasionally aborts mid-TLS-handshake.
pub(super) async fn http_auth_precheck(url: &str) -> Option<ProbeStatus> {
    let client = match reqwest::Client::builder()
        .timeout(HTTP_PRECHECK_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return None,
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "aura-mcp-probe", "version": "0"}
        },
        "id": 0
    });
    let mut last: Option<ProbeStatus> = None;
    for attempt in 0..HTTP_PRECHECK_ATTEMPTS {
        let req = client
            .post(url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&body);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                tracing::debug!(url = %url, attempt, status = %status, "mcp probe POST response");
                return match status.as_u16() {
                    401 | 403 => {
                        let realm = resp
                            .headers()
                            .get(reqwest::header::WWW_AUTHENTICATE)
                            .and_then(|h| h.to_str().ok())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("HTTP {status} (no WWW-Authenticate)"));
                        Some(ProbeStatus::AuthRequired(realm))
                    }
                    _ => None,
                };
            }
            Err(e) if e.is_timeout() => {
                tracing::debug!(url = %url, attempt, "mcp probe POST timed out");
                last = Some(ProbeStatus::Timeout);
            }
            Err(e) if e.is_connect() => {
                tracing::debug!(url = %url, attempt, error = %e, "mcp probe POST connect failed");
                last = Some(ProbeStatus::Unreachable(e.to_string()));
            }
            Err(e) => {
                tracing::debug!(url = %url, attempt, error = %e, "mcp probe POST failed (other)");
                last = None;
            }
        }
        if attempt + 1 < HTTP_PRECHECK_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    last
}

/// Best-effort classification by error-string sniffing. Returns
/// `ProbeStatus::Error` for anything that doesn't match a known pattern,
/// preserving the original message for the operator. Patterns are
/// deliberately narrow so a future rmcp release that grows typed errors
/// can replace this without flipping correct messages into wrong buckets.
fn classify(msg: String) -> ProbeStatus {
    let lower = msg.to_lowercase();
    if lower.contains(" 401 ")
        || lower.contains("(401 ")
        || lower.contains("status 401")
        || lower.contains("statuscode(401")
        || lower.contains("unauthorized")
        || lower.contains("www-authenticate")
    {
        ProbeStatus::AuthRequired(msg)
    } else if lower.contains("connection refused")
        || lower.contains("dns error")
        || lower.contains("failed to lookup")
        || lower.contains("no such host")
        || lower.contains("network is unreachable")
        || lower.contains("operation timed out")
    {
        ProbeStatus::Unreachable(msg)
    } else {
        ProbeStatus::Error(msg)
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}
