//! Drive the REAL Aura agent as a black box — the analog of how upstream's
//! benchmark drives OpenClaw: start `aura gateway` once per arm, then answer
//! each question with a concurrent `aura prompt` routed over that gateway.
//! Recall + memory tools run inside the real agent loop; the bench only execs
//! the `aura` binary (no agent-stack linkage).
//!
//! **Per-conversation isolation** rides on the `USER` env each `aura prompt`
//! inherits: `cli_user()` reads `$USER`, that becomes the message sender →
//! `session.user.id` → the recall scope OpenViking is queried under (verified
//! in `aura_agent`'s router intake). So each question runs with
//! `USER=<conv-scope>`, matching what `ingest` populated.

use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

/// Env var `aura` reads for its config path (mirrors `aura_workspace::paths`).
const AURA_CONFIG_ENV: &str = "AURA_CONFIG_PATH";
/// Gateway readiness poll cadence + ceiling. The 1h ceiling matches the bench's
/// other waits; a gateway that *crashes* is caught immediately via `try_wait`,
/// so this only bounds a process that stays alive but never binds.
const READY_POLL: Duration = Duration::from_millis(300);
const READY_TIMEOUT: Duration = Duration::from_secs(3600);
/// How much of a failed prompt's stderr to surface.
const ERR_TAIL: usize = 400;
/// Effectively-unlimited per-user rate limit for the bench gateway. QA fires
/// every question for a conversation under ONE user_id (the recall scope must
/// match ingest), far above Aura's default 30 req/60s — without lifting it, a
/// run stalls at question 30 (the rate limiter rejects the 31st in the window).
const BENCH_RATE_LIMIT_MAX_REQUESTS: usize = 1_000_000;

/// Write a per-arm config to `out_path`: load the base config JSON and replace
/// its `memory` section with `memory` (the bench's per-arm settings). Everything
/// else (llm, gateway, workspace, keys) is the user's base config untouched.
/// Returns the written path + the gateway admin address parsed from it.
pub fn prepare_arm_config(
    base_config: &Path,
    memory: serde_json::Value,
    out_path: PathBuf,
) -> Result<(PathBuf, SocketAddr)> {
    let raw = std::fs::read_to_string(base_config)
        .with_context(|| format!("read base aura config {}", base_config.display()))?;
    let mut cfg: serde_json::Value =
        serde_json::from_str(&raw).context("parse base aura config as JSON")?;

    // Overwrite the whole `memory` section with the bench's per-arm settings —
    // works whether or not the base config already has one. Everything else
    // (llm, workspace, gateway, keys) is the user's config untouched.
    cfg.as_object_mut()
        .context("base aura config is not a JSON object")?
        .insert("memory".to_string(), memory);

    let gateway = cfg
        .get("gateway")
        .and_then(|g| g.as_object())
        .context("base config has no `gateway` object")?;
    let host = gateway
        .get("bind_address")
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1");
    let port = gateway
        .get("port")
        .and_then(|v| v.as_u64())
        .context("gateway.port missing/invalid")?;
    // 0.0.0.0 is a bind wildcard, not a connect target — poll loopback.
    let connect_host = if host == "0.0.0.0" { "127.0.0.1" } else { host };
    let addr: SocketAddr = format!("{connect_host}:{port}")
        .parse()
        .with_context(|| format!("gateway addr {connect_host}:{port}"))?;

    std::fs::write(&out_path, serde_json::to_string_pretty(&cfg)?)
        .with_context(|| format!("write arm config {}", out_path.display()))?;
    Ok((out_path, addr))
}

/// Generate a fully self-contained aura config + a fresh workspace under
/// `workspace_dir` — no dependency on `~/.aura`. The only thing `aura` can't
/// bootstrap itself is the 32-byte encryption key (the vault auto-creates and
/// `aura gateway start` mints its own token), so we mint one here. The answer
/// LLM is DeepSeek keyed from `DEEPSEEK_API_KEY` (`api_key_env`); `memory` is
/// the arm's section. Returns the written config path + the gateway admin addr.
pub fn generate_config(
    workspace_dir: &Path,
    memory: serde_json::Value,
    answer_model: &str,
    answer_base_url: &str,
    gateway_port: u16,
) -> Result<(PathBuf, SocketAddr)> {
    std::fs::create_dir_all(workspace_dir)
        .with_context(|| format!("create bench workspace {}", workspace_dir.display()))?;
    // Reuse an existing key: re-running QA in a workspace that already holds a
    // vault (from a prior run) must not mint a fresh key, or the old vault can't
    // be decrypted (AES-256-GCM failure → the gateway won't start).
    let key_path = workspace_dir.join("encryption.key");
    if !key_path.exists() {
        let mut key = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut key))
            .context("mint encryption key from /dev/urandom")?;
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(&key_path, hex)
            .with_context(|| format!("write key {}", key_path.display()))?;
    }

    let workspace = workspace_dir
        .to_str()
        .context("bench workspace path is not utf-8")?;
    let key_file = key_path.to_str().context("key path is not utf-8")?;
    let cfg = serde_json::json!({
        "llm": [{
            "name": "bench-answer",
            "provider": "deepseek",
            "model": answer_model,
            "base_url": answer_base_url,
            "api_key_env": "DEEPSEEK_API_KEY",
        }],
        "default-llm": "bench-answer",
        "channels": { "cli": { "enabled": true } },
        "security": { "encryption_key_file": key_file, "leak_detection_enabled": true },
        "workspace": { "path": workspace },
        "gateway": { "bind_address": "127.0.0.1", "port": gateway_port },
        "memory": memory,
        // Lift the per-user rate limit (default 30 req/60s) so a fast single-
        // user QA run doesn't stall at question 30. Bench gateway only.
        "cost": { "rate_limit": { "max_requests": BENCH_RATE_LIMIT_MAX_REQUESTS } },
    });
    let out_path = workspace_dir.join("aura.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&cfg)?)
        .with_context(|| format!("write config {}", out_path.display()))?;
    let addr: SocketAddr = format!("127.0.0.1:{gateway_port}")
        .parse()
        .with_context(|| format!("gateway addr 127.0.0.1:{gateway_port}"))?;
    Ok((out_path, addr))
}

/// A running `aura gateway` subprocess, killed on drop. All `aura prompt`
/// invocations against it share its config (same workspace → they find its
/// admin addr + tui token).
pub struct GatewayHandle {
    child: Child,
    aura_bin: String,
    config_path: PathBuf,
}

impl GatewayHandle {
    /// Spawn `aura gateway start` with `config_path`, then poll `admin_addr`
    /// until it accepts (the listener is up + the workspace lock held, so
    /// `aura prompt` will route over WS instead of going in-process).
    pub async fn start(aura_bin: &str, config_path: &Path, admin_addr: SocketAddr) -> Result<Self> {
        let child = Command::new(aura_bin)
            .args(["gateway", "start"])
            .env(AURA_CONFIG_ENV, config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn `{aura_bin} gateway start`"))?;
        let mut handle = Self {
            child,
            aura_bin: aura_bin.to_string(),
            config_path: config_path.to_path_buf(),
        };
        handle.await_ready(admin_addr).await?;
        Ok(handle)
    }

    async fn await_ready(&mut self, addr: SocketAddr) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().context("poll gateway")? {
                bail!("aura gateway exited before becoming ready (status {status})");
            }
            if TcpStream::connect(addr).await.is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = self.child.start_kill();
                bail!("aura gateway not ready on {addr} within {READY_TIMEOUT:?}");
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Answer one prompt through the gateway: `USER=scope_user aura prompt
    /// --json --session <id> --timeout <n> [-y] -- <prompt>`. Returns the
    /// assistant's text (the `response` field of the `--json` line).
    pub async fn run_prompt(
        &self,
        scope_user: &str,
        session_id: &str,
        prompt: &str,
        timeout_secs: u64,
        allow_all: bool,
    ) -> Result<String> {
        let mut args = vec![
            "prompt".to_string(),
            "--json".to_string(),
            "--session".to_string(),
            session_id.to_string(),
            "--timeout".to_string(),
            timeout_secs.to_string(),
        ];
        if allow_all {
            args.push("-y".to_string());
        }
        // `--` so a prompt starting with `-` is not parsed as a flag.
        args.push("--".to_string());
        args.push(prompt.to_string());

        let output = Command::new(&self.aura_bin)
            .args(&args)
            .env(AURA_CONFIG_ENV, &self.config_path)
            // `cli_user()` reads $USER/$USERNAME → message sender → recall scope.
            .env("USER", scope_user)
            .env("USERNAME", scope_user)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("run `aura prompt`")?;
        if !output.status.success() {
            let tail: String = String::from_utf8_lossy(&output.stderr)
                .chars()
                .rev()
                .take(ERR_TAIL)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            bail!("aura prompt exited {}: {tail}", output.status);
        }
        parse_response(&String::from_utf8_lossy(&output.stdout))
    }

    /// Stop the gateway and reap it.
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        // Best-effort if `shutdown` wasn't called (e.g. an error path).
        let _ = self.child.start_kill();
    }
}

/// Extract the assistant text from `aura prompt --json` stdout: the last line
/// that parses as a JSON object with a `response` field. A turn the runtime
/// rejected emits `{session_id, error}` instead — surfaced as an error.
fn parse_response(stdout: &str) -> Result<String> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(answer) = value.get("response").and_then(|v| v.as_str()) {
            return Ok(answer.to_string());
        }
        if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
            bail!("aura prompt turn error: {err}");
        }
    }
    bail!(
        "no `response` in aura prompt output: {}",
        stdout.trim().chars().take(ERR_TAIL).collect::<String>()
    );
}

#[cfg(test)]
mod tests {
    use super::parse_response;

    #[test]
    fn parses_response_field_from_json_line() {
        let out = "some log line\n{\"session_id\":\"s1\",\"response\":\"a shell necklace\"}\n";
        assert_eq!(parse_response(out).unwrap(), "a shell necklace");
    }

    #[test]
    fn surfaces_error_object() {
        let out = "{\"session_id\":\"s1\",\"error\":\"budget exceeded\"}";
        let err = parse_response(out).unwrap_err().to_string();
        assert!(err.contains("budget exceeded"), "{err}");
    }

    #[test]
    fn errors_when_no_json() {
        assert!(parse_response("plain text, no json\n").is_err());
    }
}
