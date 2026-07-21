//! Parent-side capability RPCs the child calls over stdio: the
//! host-mediated `ctx.fetch` (SSRF floor + placeholder reveal + audit
//! log), `ctx.exec`, and the blob plane (`docs/modules/deck.md` §Blobs) —
//! `ctx.fetchBlob` / `ctx.blobPutFile`.
//! `ctx.fetch` keeps a raw secret from ever entering the child — the card
//! carries only the `[{REDACTED_SECRET_…}]` placeholder and the parent
//! reveals it at egress. Routing effects through here is an SDK
//! convenience (and the reveal's enforcement point), not a sandbox: the
//! child runs on the host and could open its own socket, consistent with
//! the trusted-author model.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use tokio::process::Command;

use baybo_security::{PlaceholderMinter, SecretVault};
use baybo_store::blob::{MAX_BLOB_BYTES, deck_uploader_identity};
use baybo_store::{BlobStore, ByteStream};

use crate::service::{
    HostBlobPutFileRequest, HostBlobRef, HostExecResponse, HostFetchRequest, HostFetchResponse,
    HostServices,
};

/// Wall clock for one host-mediated `ctx.fetch` (the whole request/response,
/// body included — a card fetch returns the body inline so it must complete).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// `ctx.fetchBlob` streams a possibly-large body straight to disk, so it can't
/// wear a total-request cap (a 100 MiB pull would need ~1.7 MiB/s to beat 60s).
/// It uses a connect cap plus an IDLE read cap instead: a healthy transfer of
/// any size is fine; a stalled one dies.
pub const FETCH_BLOB_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const FETCH_BLOB_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Redirects are disabled at the reqwest layer (address pinning below vets one
/// host's addresses); `ctx.fetchBlob` follows them MANUALLY so each hop is
/// re-vetted. This bounds the chain (release assets → presigned CDN is 1–2 hops).
pub const MAX_FETCH_REDIRECTS: u32 = 5;
/// Wall clock for one `ctx.exec` command.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// stdout/stderr cap for one `ctx.exec` command (each, after which the
/// stream is truncated with a marker).
pub const EXEC_OUTPUT_MAX: usize = 4 * 1024 * 1024;
/// Default mime when neither the card nor the response/extension names one.
const DEFAULT_BLOB_MIME: &str = "application/octet-stream";

/// Env var injected into every `ctx.exec` naming a per-card directory
/// (`<workspace>/deck/tmux-socks/<card_id>`) where a card should place its
/// own tmux socket (`tmux -S "$BAYBO_DECK_TMUX_DIR/<name>.sock"`). It keeps
/// an agent-driven CLI's tmux server off the user's default socket
/// (`/tmp/tmux-<uid>/default`, so out of their `tmux ls`) and out of
/// `/tmp` (so a `/tmp` wipe can't kill it); the per-card subdir lets purge
/// reap exactly the departing card's servers. Documented in the `deck`
/// skill.
pub const DECK_TMUX_DIR_ENV: &str = "BAYBO_DECK_TMUX_DIR";
const TMUX_SOCKS_SUBDIR: &str = "tmux-socks";

pub(crate) struct DeckHost {
    vault: Arc<SecretVault>,
    /// Scratch root for exec working dirs (per card).
    scratch_root: PathBuf,
    /// Shared blob store — the same one chat attachments use. Deck blobs are
    /// stamped `deck:<card_id>` so GC can find them without touching chat data.
    blob: Arc<dyn BlobStore>,
    /// `<deck_root>/tmux-socks` — each card's `DECK_TMUX_DIR_ENV` is a
    /// subdir of this keyed by card id.
    tmux_socks_root: PathBuf,
}

impl DeckHost {
    pub fn new(
        vault: Arc<SecretVault>,
        scratch_root: PathBuf,
        blob: Arc<dyn BlobStore>,
        deck_root: &Path,
    ) -> Self {
        Self {
            vault,
            scratch_root,
            blob,
            tmux_socks_root: deck_root.join(TMUX_SOCKS_SUBDIR),
        }
    }

    /// The card's private tmux socket dir — what exec exports as
    /// `DECK_TMUX_DIR_ENV`, and what the runtime reap (purge, gate finish)
    /// clears.
    pub(crate) fn tmux_dir(&self, card_id: &str) -> PathBuf {
        self.tmux_socks_root.join(card_id)
    }

    /// `<deck_root>/tmux-socks` — what the boot orphan sweep walks.
    pub(crate) fn tmux_socks_root(&self) -> &Path {
        &self.tmux_socks_root
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
        let (host, addrs) = vet_url(&url).await?;
        // SSRF floor: literal IPs were checked directly by `vet_url`;
        // hostnames were resolved there and the connection is pinned to the
        // vetted addresses so a rebinding resolver can't swap them later.
        let client = build_client(&host, &addrs, |b| b.timeout(FETCH_TIMEOUT))?;

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

    async fn run_fetch_blob(
        &self,
        uploader_card_id: &str,
        req: HostFetchRequest,
    ) -> std::result::Result<HostBlobRef, String> {
        // Reveal card-authored inputs once — they don't change across hops
        // (a redirect target comes from the server's Location, not the card).
        let (url_str, mut revealed) = self.reveal(&req.url).await;
        let mut url = url::Url::parse(&url_str).map_err(|e| format!("invalid URL: {e}"))?;
        let method = req.method.as_deref().unwrap_or("GET").to_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("invalid method `{method}`"))?;
        // Header names (lowercased) that must NOT survive a cross-host redirect:
        // the always-sensitive ones plus any the card revealed a vault secret
        // into. Redirects are followed manually below (Policy::none), which
        // bypasses reqwest's own cross-origin sensitive-header stripping — so a
        // server that 302s to another host would otherwise receive the secret.
        let mut sensitive: std::collections::HashSet<String> =
            ["authorization", "cookie", "proxy-authorization"]
                .into_iter()
                .map(str::to_string)
                .collect();
        let mut headers: Vec<(String, String)> = Vec::with_capacity(req.headers.len());
        for (name, value) in &req.headers {
            let (value, r) = self.reveal(value).await;
            revealed |= r;
            if r {
                sensitive.insert(name.to_ascii_lowercase());
            }
            headers.push((name.clone(), value));
        }
        let body = match &req.body {
            Some(b) => {
                let (b, r) = self.reveal(b).await;
                revealed |= r;
                Some(b)
            }
            None => None,
        };

        let mut hops = 0u32;
        loop {
            let (host, addrs) = vet_url(&url).await?;
            let client = build_client(&host, &addrs, |b| {
                b.connect_timeout(FETCH_BLOB_CONNECT_TIMEOUT)
                    .read_timeout(FETCH_BLOB_READ_IDLE_TIMEOUT)
            })?;
            let mut builder = client.request(method.clone(), url.clone());
            for (name, value) in &headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = &body {
                builder = builder.body(body.clone());
            }
            if revealed {
                tracing::info!(card = %uploader_card_id, host = %host, "deck: secret-bearing egress (fetchBlob)");
            }

            let resp = builder.send().await.map_err(|e| format!("fetch: {e}"))?;
            let status = resp.status();
            if status.is_redirection() {
                if hops >= MAX_FETCH_REDIRECTS {
                    return Err(format!(
                        "fetchBlob: too many redirects (> {MAX_FETCH_REDIRECTS})"
                    ));
                }
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or("fetchBlob: redirect without a Location header")?;
                let prev_origin = url.origin();
                url = url
                    .join(loc)
                    .map_err(|e| format!("fetchBlob: bad redirect target: {e}"))?;
                // Cross-ORIGIN redirect: drop credential-bearing headers before the
                // next hop (matches reqwest's automatic cross-origin stripping,
                // which the manual follow above bypasses). Compare the full origin
                // — scheme + host + port — so an https→http downgrade or a hop to a
                // different port (another service) strips too, not only a different
                // host. A presigned-CDN target (the common release-asset case)
                // carries its auth in the query, not a header, so this is fine.
                if url.origin() != prev_origin {
                    headers.retain(|(name, _)| !sensitive.contains(&name.to_ascii_lowercase()));
                }
                hops += 1;
                continue;
            }
            // Only a 2xx body is stored — a 4xx/5xx error page must not be
            // handed back as a successful-looking blob.
            if !status.is_success() {
                return Err(format!("fetchBlob: status {}", status.as_u16()));
            }
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(';').next())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_BLOB_MIME)
                .to_string();
            let stream = resp.bytes_stream().map_err(std::io::Error::other);
            let boxed: ByteStream = stream.boxed();
            let blob = self
                .blob
                .put_stream(
                    boxed,
                    &content_type,
                    Some(&deck_uploader_identity(uploader_card_id)),
                    MAX_BLOB_BYTES as u64,
                )
                .await
                .map_err(|e| format!("fetchBlob: store: {e}"))?;
            let size = self
                .blob
                .stat(&blob.blob_id)
                .await
                .map(|m| m.size)
                .unwrap_or(0);
            return Ok(HostBlobRef {
                blob_id: blob.blob_id,
                content_type,
                size,
            });
        }
    }
}

/// Vet a URL against the SSRF floor. Returns the host plus the vetted socket
/// addresses to pin the connection to — an EMPTY vec means the host was a
/// literal IP already checked, so no `resolve_to_addrs` pin is needed.
async fn vet_url(url: &url::Url) -> std::result::Result<(String, Vec<SocketAddr>), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme `{other}`")),
    }
    let host = url.host_str().ok_or("URL has no host")?.to_string();
    let port = url
        .port_or_known_default()
        .ok_or("URL has no usable port")?;
    match host.parse::<IpAddr>() {
        Ok(ip) => {
            if baybo_security::is_blocked_ip(&ip, false) {
                return Err(format!("blocked address: {ip}"));
            }
            Ok((host, Vec::new()))
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
            Ok((host, addrs))
        }
    }
}

/// Build a redirect-disabled client pinned to `addrs` (the SSRF floor), with
/// per-call timeouts applied by `configure`.
fn build_client(
    host: &str,
    addrs: &[SocketAddr],
    configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> std::result::Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if !addrs.is_empty() {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    configure(builder)
        .build()
        .map_err(|e| format!("client: {e}"))
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

/// Best-effort mime from a file extension for `blobPutFile` when the card
/// names none. Unknown extensions fall back to octet-stream.
fn guess_mime_from_path(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "csv" => "text/csv",
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        _ => DEFAULT_BLOB_MIME,
    };
    mime.to_string()
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
            .env(DECK_TMUX_DIR_ENV, self.tmux_dir(card_id))
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

    async fn fetch_blob(
        &self,
        uploader_card_id: &str,
        req: HostFetchRequest,
    ) -> std::result::Result<HostBlobRef, String> {
        self.run_fetch_blob(uploader_card_id, req).await
    }

    async fn blob_put_file(
        &self,
        card_id: &str,
        uploader_card_id: &str,
        req: HostBlobPutFileRequest,
    ) -> std::result::Result<HostBlobRef, String> {
        // Relative paths resolve against the exec scratch cwd (where an
        // `ctx.exec`-produced file lands); absolute paths are allowed
        // (trusted author). A `..` climb out of scratch is a footgun, not an
        // attack — reject it with a clear message.
        let path = {
            let p = Path::new(&req.path);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                if p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(format!(
                        "blobPutFile: a relative path must stay within scratch (no `..`): {}",
                        req.path
                    ));
                }
                self.scratch_root.join(card_id).join(p)
            }
        };
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| format!("blobPutFile: {}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("blobPutFile: not a file: {}", path.display()));
        }
        if meta.len() > MAX_BLOB_BYTES as u64 {
            return Err(format!(
                "blobPutFile: {} bytes exceeds the cap ({MAX_BLOB_BYTES})",
                meta.len()
            ));
        }
        let content_type = req
            .content_type
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| guess_mime_from_path(&path));
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| format!("blobPutFile: open {}: {e}", path.display()))?;
        let boxed: ByteStream = tokio_util::io::ReaderStream::new(file).boxed();
        let blob = self
            .blob
            .put_stream(
                boxed,
                &content_type,
                Some(&deck_uploader_identity(uploader_card_id)),
                MAX_BLOB_BYTES as u64,
            )
            .await
            .map_err(|e| format!("blobPutFile: store: {e}"))?;
        let size = self
            .blob
            .stat(&blob.blob_id)
            .await
            .map(|m| m.size)
            .unwrap_or(meta.len());
        Ok(HostBlobRef {
            blob_id: blob.blob_id,
            content_type,
            size,
        })
    }
}
