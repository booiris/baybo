//! Parent-side capability RPCs the child calls over stdio: the
//! host-mediated `ctx.fetch` (SSRF floor + placeholder reveal + audit
//! log) and `ctx.exec`. `ctx.fetch` keeps a raw secret from ever
//! entering the child — the card carries only the `[{REDACTED_SECRET_…}]`
//! placeholder and the parent reveals it at egress. Routing effects
//! through here is an SDK convenience (and the reveal's enforcement
//! point), not a sandbox: the child runs on the host and could open its
//! own socket, consistent with the trusted-author model.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use baybo_security::{PlaceholderMinter, SecretVault};

use crate::service::{HostExecResponse, HostFetchRequest, HostFetchResponse, HostServices};

/// Wall clock for one host-mediated fetch.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// Wall clock for one `ctx.exec` command.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// stdout/stderr cap for one `ctx.exec` command (each, after which the
/// stream is truncated with a marker).
pub const EXEC_OUTPUT_MAX: usize = 4 * 1024 * 1024;

pub(crate) struct DeckHost {
    vault: Arc<SecretVault>,
    /// Scratch root for exec working dirs (per card).
    scratch_root: PathBuf,
}

impl DeckHost {
    pub fn new(vault: Arc<SecretVault>, scratch_root: PathBuf) -> Self {
        Self {
            vault,
            scratch_root,
        }
    }

    /// Replace every `[{REDACTED_SECRET_…}]` placeholder with the vault
    /// value it names. Returns whether any reveal happened (the audit
    /// signal). Unknown placeholders are left in place, matching the
    /// agent-side reveal semantics.
    async fn reveal(&self, text: &str) -> (String, bool) {
        let re = PlaceholderMinter::placeholder_regex();
        let placeholders: Vec<String> =
            re.find_iter(text).map(|m| m.as_str().to_string()).collect();
        if placeholders.is_empty() {
            return (text.to_string(), false);
        }
        let mut out = text.to_string();
        let mut revealed = false;
        for ph in placeholders {
            match self.vault.get_secret(&ph).await {
                Ok(Some(secret)) => {
                    let plain = secret.as_str().map(str::to_owned).unwrap_or_else(|_| {
                        String::from_utf8_lossy(secret.as_bytes()).into_owned()
                    });
                    out = out.replace(&ph, &plain);
                    revealed = true;
                }
                Ok(None) => {
                    tracing::warn!("deck: reveal requested for unknown placeholder");
                }
                Err(e) => {
                    tracing::warn!("deck: vault error during reveal: {e}");
                }
            }
        }
        (out, revealed)
    }

    async fn fetch_external(
        &self,
        card_id: &str,
        req: HostFetchRequest,
    ) -> std::result::Result<HostFetchResponse, String> {
        // Reveal placeholders in URL, headers, and body before parsing so
        // a secret-bearing query param still yields a valid URL.
        let (url_str, mut revealed) = self.reveal(&req.url).await;
        let url = url::Url::parse(&url_str).map_err(|e| format!("invalid URL: {e}"))?;
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(format!("unsupported scheme `{other}`")),
        }
        let host = url.host_str().ok_or("URL has no host")?.to_string();
        let port = url
            .port_or_known_default()
            .ok_or("URL has no usable port")?;

        // SSRF floor: literal IPs are checked directly; hostnames are
        // resolved here and the connection pinned to the vetted
        // addresses so a rebinding resolver can't swap them later.
        let mut client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT);
        match host.parse::<IpAddr>() {
            Ok(ip) => {
                if baybo_security::is_blocked_ip(&ip, false) {
                    return Err(format!("blocked address: {ip}"));
                }
            }
            Err(_) => {
                let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
                    .await
                    .map_err(|e| format!("DNS lookup failed for {host}: {e}"))?
                    .filter(|a| !baybo_security::is_blocked_ip(&a.ip(), false))
                    .collect();
                if addrs.is_empty() {
                    return Err(format!("{host} resolves only to blocked addresses"));
                }
                client = client.resolve_to_addrs(&host, &addrs);
            }
        }
        let client = client.build().map_err(|e| format!("client: {e}"))?;

        let method = req.method.as_deref().unwrap_or("GET").to_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("invalid method `{method}`"))?;
        let mut builder = client.request(method, url);
        for (name, value) in &req.headers {
            let (value, r) = self.reveal(value).await;
            revealed |= r;
            builder = builder.header(name, value);
        }
        if let Some(body) = &req.body {
            let (body, r) = self.reveal(body).await;
            revealed |= r;
            builder = builder.body(body);
        }

        if revealed {
            // The audit trail for secret-bearing egress: card id + host,
            // never the secret or the full URL.
            tracing::info!(card = %card_id, host = %host, "deck: secret-bearing egress");
        }

        let resp = builder.send().await.map_err(|e| format!("fetch: {e}"))?;
        let status = resp.status().as_u16();
        let headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        // No response-body cap by design: a card's service is trusted
        // author code (see docs/modules/deck.md), and cards that back a
        // game / rich frontend may legitimately pull large payloads.
        let mut body: Vec<u8> = Vec::new();
        let mut resp = resp;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("body: {e}"))? {
            body.extend_from_slice(&chunk);
        }
        Ok(HostFetchResponse {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }
}

fn truncate_output(bytes: &[u8]) -> String {
    if bytes.len() <= EXEC_OUTPUT_MAX {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let mut s = String::from_utf8_lossy(&bytes[..EXEC_OUTPUT_MAX]).into_owned();
        s.push_str("\n[truncated]");
        s
    }
}

#[async_trait]
impl HostServices for DeckHost {
    async fn fetch(
        &self,
        card_id: &str,
        req: HostFetchRequest,
    ) -> std::result::Result<HostFetchResponse, String> {
        self.fetch_external(card_id, req).await
    }

    async fn exec(
        &self,
        card_id: &str,
        cmd: String,
    ) -> std::result::Result<HostExecResponse, String> {
        if cmd.trim().is_empty() {
            return Err("empty command".into());
        }
        let scratch = self.scratch_root.join(card_id);
        if let Err(e) = std::fs::create_dir_all(&scratch) {
            return Err(format!("scratch dir: {e}"));
        }
        // Runs on the host with the inherited environment (so installed
        // CLIs and credential dirs resolve), in a per-card scratch cwd.
        // The 10s wall clock + output caps are the only bounds; a
        // misbehaving card is caught by the strike/quarantine budget, not
        // an OS jail (trusted-author model — see `docs/modules/deck.md`).
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&cmd)
            .current_dir(&scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| format!("exec: {e}"))?;
        let out = match tokio::time::timeout(EXEC_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(format!("exec: {e}")),
            Err(_) => return Err(format!("exec: timed out after {EXEC_TIMEOUT:?}")),
        };
        Ok(HostExecResponse {
            code: out.status.code().unwrap_or(-1),
            stdout: truncate_output(&out.stdout),
            stderr: truncate_output(&out.stderr),
        })
    }
}
