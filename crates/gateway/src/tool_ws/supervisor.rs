//! Restart loop for the browser tool sidecar (a bundled bun + Playwright
//! process). Mirrors the channel `SidecarSupervisor` in shape (lazy
//! spawn, exponential backoff, SIGKILL on shutdown) but trimmed for
//! tool sidecars: no ChannelBotStore poll, no per-channel log dir,
//! and lifetime-bound to the gateway (always alive while the gateway
//! is up, regardless of agent activity).
//!
//! When the operator sets `tools.browser.external_url`, the supervisor
//! is *not* started — an external process (e.g. a separate Docker
//! container) connects to `/v1/tool-ws` on its own.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_agent::service::ShutdownSignal;
use aura_cron::Shutdown;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::time::sleep;

/// Override for the `node` binary used to run tool sidecars. The
/// tool-sidecar bundle is built with `esbuild --target=node` and
/// uses the npm `ws` package whose Node-specific request shaping
/// the channel-side bun runtime breaks (it returns
/// `Unexpected server response: 101` when bundled). Channel
/// sidecars keep using bun via `AURA_BUN_BIN`; this is a separate
/// knob for the tool surface only.
pub const NODE_BINARY_ENV: &str = "AURA_NODE_BIN";

/// Resolve the `node` binary used to run tool sidecars.
fn node_binary() -> PathBuf {
    std::env::var_os(NODE_BINARY_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node"))
}

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const UPTIME_RESET_THRESHOLD: Duration = Duration::from_secs(60);
const PIPE_LINE_MAX_BYTES: usize = 1024;

/// Configuration for the tool-sidecar supervisor.
#[derive(Clone)]
pub struct ToolSidecarConfig {
    /// Path to the bundled `bundle.mjs` (resolved by
    /// [`super::super::sidecar::assets::SidecarRuntime::bundle_for("browser")`]).
    pub bundle_path: PathBuf,
    /// `ws://…/v1/tool-ws` URL the sidecar should connect back to.
    pub ws_url: String,
    /// Pre-shared registration token (matches `register_token` in
    /// [`super::server::ToolWsState`]).
    pub ws_token: String,
    /// Optional `playwright.chromium.executablePath` override; threaded
    /// through to the child as `AURA_CHROMIUM_BIN`. Empty / `None`
    /// lets Playwright pick its bundled binary.
    pub chromium_executable: Option<PathBuf>,
    /// Per-Aura-session profile dir override. `None` lets the sidecar
    /// fall back to its `$XDG_CACHE_HOME/aura/browser/profile` default.
    pub profile_dir: Option<PathBuf>,
    /// When true, launch Chromium with `--no-sandbox`. Default false.
    /// Operators who already run the gateway under an outer sandbox
    /// (rootless docker, gVisor, …) can opt in via the
    /// `AURA_BROWSER_NO_SANDBOX` env var on the gateway process — the
    /// caller maps that into this field at startup.
    pub no_sandbox: bool,
}

#[derive(Clone)]
pub struct ToolSidecarSupervisor {
    config: Arc<ToolSidecarConfig>,
}

impl ToolSidecarSupervisor {
    pub fn new(config: ToolSidecarConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Run the restart loop until `shutdown` fires.
    pub async fn run(self, shutdown: ShutdownSignal) {
        let mut backoff = BACKOFF_MIN;
        loop {
            if shutdown.is_triggered() {
                return;
            }
            let started_at = Instant::now();
            let node = node_binary();
            let mut cmd = Command::new(&node);
            cmd.arg(&self.config.bundle_path);
            // Start with a clean env and re-add only what the sidecar
            // strictly needs. Inheriting the gateway's full env leaks
            // RUST_LOG, vault paths, and any host secrets into the
            // child's `/proc/<pid>/environ`, widening the blast
            // radius of a renderer escape. PATH is required so node
            // can still resolve dynamic linker stubs and any system
            // tools playwright invokes (uname, getconf, ...). HOME +
            // XDG_CACHE_HOME are needed so the sidecar can compute
            // its profile dir under $XDG_CACHE_HOME/aura/...
            cmd.env_clear();
            for key in ["PATH", "HOME", "USER", "LANG", "XDG_CACHE_HOME", "TMPDIR"] {
                if let Ok(v) = std::env::var(key) {
                    cmd.env(key, v);
                }
            }
            cmd.env("AURA_TOOL_WS_URL", &self.config.ws_url);
            cmd.env("AURA_TOOL_WS_TOKEN", &self.config.ws_token);
            cmd.env("AURA_TOOL_SIDECAR_ID", crate::sidecar::domains::BROWSER);
            if let Some(p) = &self.config.chromium_executable {
                cmd.env("AURA_CHROMIUM_BIN", p);
            }
            if let Some(p) = &self.config.profile_dir {
                cmd.env("AURA_BROWSER_PROFILE_DIR", p);
            }
            if self.config.no_sandbox {
                cmd.env("AURA_BROWSER_NO_SANDBOX", "1");
            }
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            cmd.kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        bin = %node.display(),
                        bundle = %self.config.bundle_path.display(),
                        "tool sidecar spawn failed",
                    );
                    if !back_off(&mut backoff, &shutdown).await {
                        return;
                    }
                    continue;
                }
            };

            let pid = child.id();
            tracing::info!(pid, bundle = %self.config.bundle_path.display(), "browser tool sidecar started");

            let stdout_handle = child
                .stdout
                .take()
                .map(|s| spawn_pipe_pump(s, "browser-sidecar/stdout"));
            let stderr_handle = child
                .stderr
                .take()
                .map(|s| spawn_pipe_pump(s, "browser-sidecar/stderr"));

            tokio::select! {
                _ = shutdown.wait() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    if let Some(h) = stdout_handle { h.abort(); }
                    if let Some(h) = stderr_handle { h.abort(); }
                    return;
                }
                wait_res = child.wait() => {
                    match wait_res {
                        Ok(status) => tracing::warn!(?status, "browser tool sidecar exited"),
                        Err(e) => tracing::warn!(error = %e, "browser tool sidecar wait failed"),
                    }
                    if let Some(h) = stdout_handle { h.abort(); }
                    if let Some(h) = stderr_handle { h.abort(); }
                }
            }

            if started_at.elapsed() >= UPTIME_RESET_THRESHOLD {
                backoff = BACKOFF_MIN;
            }
            if !back_off(&mut backoff, &shutdown).await {
                return;
            }
        }
    }
}

async fn back_off(backoff: &mut Duration, shutdown: &ShutdownSignal) -> bool {
    let cur = *backoff;
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
    tokio::select! {
        _ = sleep(cur) => true,
        _ = shutdown.wait() => false,
    }
}

/// Hard cap on bytes accumulated into a single log line before we
/// force a flush. `BufReader::lines()` itself has no per-line cap
/// and would happily buffer a multi-GB unbroken stream into one
/// allocation before yielding — a misbehaving sidecar dumping a
/// no-newline blast OOMs the supervisor before `PIPE_LINE_MAX_BYTES`
/// truncation runs. The fill_buf / consume loop below caps it.
const PIPE_LINE_READ_BUF_MAX: usize = 64 * 1024;

fn spawn_pipe_pump<R: AsyncRead + Unpin + Send + 'static>(
    pipe: R,
    target: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        loop {
            buf.clear();
            let outcome = read_capped_line(&mut reader, &mut buf).await;
            match outcome {
                Ok(LineOutcome::Eof) => return,
                Ok(LineOutcome::Newline) | Ok(LineOutcome::CapHit) => {}
                Err(e) => {
                    tracing::debug!(error = %e, subsystem = target, "tool sidecar pipe read failed");
                    return;
                }
            }
            // Strip trailing newline for cleaner log output.
            if buf.last() == Some(&b'\n') {
                buf.pop();
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
            }
            let raw = String::from_utf8_lossy(&buf);
            let cap_hit = matches!(outcome, Ok(LineOutcome::CapHit));
            let trimmed = if raw.len() > PIPE_LINE_MAX_BYTES {
                let mut cut = PIPE_LINE_MAX_BYTES;
                while cut > 0 && !raw.is_char_boundary(cut) {
                    cut -= 1;
                }
                let mut s = raw[..cut].to_owned();
                s.push_str(if cap_hit {
                    " [...truncated, line exceeded read cap]"
                } else {
                    " [...truncated]"
                });
                s
            } else if cap_hit {
                let mut s = raw.into_owned();
                s.push_str(" [...truncated, line exceeded read cap]");
                s
            } else {
                raw.into_owned()
            };
            tracing::info!(target: "tool_sidecar", subsystem = target, "{trimmed}");

            // After a cap-hit flush, drain bytes until the next
            // newline so we resync at a line boundary instead of
            // emitting the next chunk as if it were a new line.
            if cap_hit {
                let _ = drain_until_newline(&mut reader).await;
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOutcome {
    /// Reached `\n` within the cap.
    Newline,
    /// Hit `PIPE_LINE_READ_BUF_MAX` first; caller flushes + drains.
    CapHit,
    /// EOF before any data on this iteration.
    Eof,
}

/// Read into `out` until either `\n` or the byte cap. Uses
/// `fill_buf` / `consume` so we never allocate beyond the cap.
async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<LineOutcome> {
    while out.len() < PIPE_LINE_READ_BUF_MAX {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Ok(if out.is_empty() {
                LineOutcome::Eof
            } else {
                LineOutcome::Newline
            });
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            let take = (pos + 1).min(PIPE_LINE_READ_BUF_MAX - out.len());
            out.extend_from_slice(&avail[..take]);
            reader.consume(take);
            return Ok(LineOutcome::Newline);
        }
        let take = avail.len().min(PIPE_LINE_READ_BUF_MAX - out.len());
        out.extend_from_slice(&avail[..take]);
        reader.consume(take);
    }
    Ok(LineOutcome::CapHit)
}

/// Discard input up to and including the next `\n`, so a cap-hit
/// flush can resume on a clean line boundary.
async fn drain_until_newline<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<()> {
    loop {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Ok(());
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            reader.consume(pos + 1);
            return Ok(());
        }
        let len = avail.len();
        reader.consume(len);
    }
}
