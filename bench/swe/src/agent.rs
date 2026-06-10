//! Drive the REAL aura agent inside each instance's **official SWE-bench Docker
//! image** — the faithful, leaderboard-comparable way to measure issue
//! resolution. Per instance we `docker run` the eval image (the repo checked out
//! at `base_commit` at `/testbed`, all deps installed), copy in an `aura` binary
//! built with `--features bench-passthrough` plus a `sandbox.mode = passthrough`
//! config, run `aura prompt` with cwd `/testbed`, then capture `git diff` as the
//! prediction and read the turn's cost from aura's ledger.
//!
//! Passthrough is mandatory here: aura's normal OS sandbox (bwrap) can't nest in
//! the container, and the container is already the isolation boundary. The grader
//! runs separately, in its own hermetic containers — see [`crate::grader`].

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::SweInstance;

/// Container paths. The aura state dir is **outside** `/testbed` so its vault /
/// sessions / logs never show up in the captured `git diff`.
const CONTAINER_AURA_DIR: &str = "/installed-agent";
const CONTAINER_AURA_BIN: &str = "/installed-agent/aura";
const CONTAINER_CONFIG: &str = "/installed-agent/aura.json";
const CONTAINER_KEY_FILE: &str = "/installed-agent/encryption.key";
const CONTAINER_WORKSPACE: &str = "/aura-home";
/// SWE-bench eval images check the repo out here.
const TESTBED: &str = "/testbed";
/// SWE-bench images install the repo (editable, at base_commit) into a conda env
/// named `testbed`; bare `python` is the base env with no project installed. We
/// launch aura through a login shell that activates it, so the agent's
/// `python`/`pip` resolve to the right interpreter and it can actually run the
/// project's tests to verify its fix. Best-effort: if activation fails (a
/// non-conda image), `exec "$@"` still runs aura directly.
const CONDA_ACTIVATE_WRAP: &str = "source /opt/miniconda3/etc/profile.d/conda.sh 2>/dev/null; \
     conda activate testbed 2>/dev/null; exec \"$@\"";
/// Effectively-unlimited per-user rate limit for the bench (one user, one turn).
const BENCH_RATE_LIMIT_MAX_REQUESTS: u64 = 1_000_000;
const ERR_TAIL: usize = 600;

/// The LLM the in-container agent answers with. A `None` `base_url` is omitted
/// so the provider uses its built-in endpoint.
pub struct AgentModel {
    pub provider: String,
    pub model: String,
    pub api_key_env: String,
    pub base_url: Option<String>,
}

/// Render the in-container `aura.json`: passthrough sandbox, the agent model,
/// an isolated workspace, and a lifted rate limit. Returned as text to pipe into
/// the container (no host temp file).
pub fn render_agent_config(model: &AgentModel) -> String {
    let mut llm = serde_json::json!({
        "name": "bench",
        "provider": model.provider,
        "model": model.model,
        "api_key_env": model.api_key_env,
    });
    if let Some(base_url) = &model.base_url {
        llm["base_url"] = serde_json::json!(base_url);
    }
    let cfg = serde_json::json!({
        "llm": [llm],
        "default-llm": "bench",
        "channels": { "cli": { "enabled": true } },
        "security": { "encryption_key_file": CONTAINER_KEY_FILE, "leak_detection_enabled": true },
        "workspace": { "path": CONTAINER_WORKSPACE },
        // The whole point of this build: run Bash unsandboxed in the container.
        "sandbox": { "mode": "passthrough" },
        "cost": { "rate_limit": { "max_requests": BENCH_RATE_LIMIT_MAX_REQUESTS } },
    });
    serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".to_string())
}

/// One instance's agent run. `error` is set (and `patch` likely empty) on any
/// failure; the harness then grades it as unresolved rather than the run aborting.
#[derive(Debug, Clone)]
pub struct InstanceRun {
    pub instance_id: String,
    pub patch: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: i64,
    /// Wall time of the `aura prompt` turn (the agent's actual work).
    pub latency_ms: u64,
    pub error: Option<String>,
}

impl InstanceRun {
    fn failed(instance_id: &str, error: String) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            patch: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cost_micro_usd: 0,
            latency_ms: 0,
            error: Some(error),
        }
    }
}

/// Inputs for one instance's containerized agent run.
pub struct RunOpts<'a> {
    pub docker_bin: &'a str,
    /// Host path of the `aura` binary (built `--features bench-passthrough`).
    pub aura_bin_host: &'a Path,
    /// Rendered `aura.json` text (see [`render_agent_config`]).
    pub config_json: &'a str,
    pub instance: &'a SweInstance,
    pub api_key_env: &'a str,
    pub api_key_value: &'a str,
    pub session_id: String,
    pub container_name: String,
    pub prompt_timeout_secs: u64,
}

/// Whether a Docker image is already present locally.
pub async fn image_exists(docker_bin: &str, image_key: &str) -> bool {
    Command::new(docker_bin)
        .args(["image", "inspect", image_key])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pull an image (e.g. a prebuilt `swebench/...` Hub image), streaming progress.
/// Returns whether it succeeded. A local-build image name (no Hub counterpart)
/// will fail here — the caller then points the user at `prepare_images`.
pub async fn pull_image(docker_bin: &str, image_key: &str) -> bool {
    Command::new(docker_bin)
        .args(["pull", image_key])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run one instance end-to-end. Tears the container down by default; never
/// returns `Err` (per-instance failures become `InstanceRun.error`). Set
/// `BENCH_KEEP_CONTAINERS=1` to leave the container up for debugging (e.g. to
/// export aura's session transcript from `/aura-home/state/storage.db`).
pub async fn run_instance(opts: RunOpts<'_>) -> InstanceRun {
    let result = run_instance_inner(&opts).await;
    if std::env::var_os("BENCH_KEEP_CONTAINERS").is_some() {
        tracing::warn!(
            container = %opts.container_name,
            "BENCH_KEEP_CONTAINERS set — leaving the container up (debug); remove it manually"
        );
    } else {
        // Always reap the container, even on the error path.
        let _ = docker(opts.docker_bin, &["rm", "-f", &opts.container_name]).await;
    }
    match result {
        Ok(run) => run,
        Err(e) => InstanceRun::failed(&opts.instance.instance_id, err_tail(&e.to_string())),
    }
}

async fn run_instance_inner(opts: &RunOpts<'_>) -> Result<InstanceRun> {
    let name = opts.container_name.as_str();
    let instance = opts.instance;

    // 1. Start the eval image detached. `--cap-add` mirrors what the grader
    //    applies (swebench `build_container` reads `docker_specs.run_args.cap_add`),
    //    so the agent's container matches grading's runtime. The API key is passed
    //    by NAME only (`-e NAME`, value in the docker process env) so the secret
    //    never lands in argv / `ps`; the container env then carries it to every exec.
    let mut run_args: Vec<&str> = vec!["run", "-d", "--name", name];
    for cap in &instance.cap_add {
        run_args.push("--cap-add");
        run_args.push(cap.as_str());
    }
    run_args.push("-e");
    run_args.push(opts.api_key_env);
    run_args.push(instance.image_key.as_str());
    run_args.push("sleep");
    run_args.push("infinity");
    docker_env(
        opts.docker_bin,
        &run_args,
        &[(opts.api_key_env, opts.api_key_value)],
    )
    .await
    .with_context(|| format!("docker run {}", instance.image_key))?;

    // 2. Lay down the aura dir, binary, config, and a fresh encryption key.
    docker(
        opts.docker_bin,
        &[
            "exec",
            name,
            "mkdir",
            "-p",
            CONTAINER_AURA_DIR,
            CONTAINER_WORKSPACE,
        ],
    )
    .await?;
    let host_bin = opts
        .aura_bin_host
        .to_str()
        .context("aura binary path is not utf-8")?;
    docker(
        opts.docker_bin,
        &["cp", host_bin, &format!("{name}:{CONTAINER_AURA_BIN}")],
    )
    .await
    .context("docker cp aura binary into container")?;
    docker(
        opts.docker_bin,
        &["exec", name, "chmod", "+x", CONTAINER_AURA_BIN],
    )
    .await?;
    docker_with_stdin(
        opts.docker_bin,
        &[
            "exec",
            "-i",
            name,
            "sh",
            "-c",
            &format!("cat > {CONTAINER_CONFIG}"),
        ],
        opts.config_json,
    )
    .await
    .context("write aura.json into container")?;
    docker(
        opts.docker_bin,
        &[
            "exec",
            name,
            "sh",
            "-c",
            &format!("head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \\n' > {CONTAINER_KEY_FILE}"),
        ],
    )
    .await
    .context("mint encryption key in container")?;

    // 3. Run the agent: cwd /testbed so it edits the repo in place. `-y` auto-
    //    approves writes outside `work/`; `--timeout` bounds the turn.
    let framed = crate::frame_instruction(instance);
    let timeout_str = opts.prompt_timeout_secs.to_string();
    let prompt_args: Vec<&str> = vec![
        "exec",
        "-w",
        TESTBED,
        "-e",
        "AURA_CONFIG_PATH=/installed-agent/aura.json",
        // The API key is NOT re-passed here — the container already has it (from
        // `docker run -e NAME`), and exec inherits the container env. Keeps the
        // secret out of this argv too.
        name,
        // `bash -lc '<activate>; exec "$@"' aura <cmd...>`: activate the testbed
        // conda env, then exec aura. The aura command + the framed instruction
        // ride in as positionals ($1…), so no re-quoting and the multi-line
        // instruction stays a single safe argv.
        "bash",
        "-lc",
        CONDA_ACTIVATE_WRAP,
        "aura",
        CONTAINER_AURA_BIN,
        "prompt",
        "--json",
        "-y",
        "--session",
        &opts.session_id,
        "--timeout",
        &timeout_str,
        "--",
        &framed,
    ];
    let started = Instant::now();
    let (stdout, stderr, success) = docker_capture(
        opts.docker_bin,
        &prompt_args,
        // Hard safety net above aura's own `--timeout`, in case the turn wedges.
        Duration::from_secs(opts.prompt_timeout_secs + 120),
    )
    .await?;
    let latency_ms = started.elapsed().as_millis() as u64;
    let mut prompt_error = if !success {
        Some(format!(
            "aura prompt exited non-zero: {}",
            err_tail(&stderr)
        ))
    } else {
        prompt_output_error(&stdout)
    };

    // 4. Capture the prediction: the agent's changes on top of the image's
    //    current HEAD — NOT the dataset `base_commit`. SWE-bench eval images sit
    //    on a "SWE-bench" setup commit *above* base_commit (e.g. pinning build
    //    deps like `setuptools` in pyproject.toml so the old tree installs);
    //    diffing against base_commit folds that image setup into the prediction
    //    and breaks grading. `add -A -N` includes new files (intent-to-add so
    //    they show in `diff`).
    let diff_cmd =
        format!("git -C {TESTBED} add -A -N >/dev/null 2>&1; git -C {TESTBED} diff HEAD");
    let patch = match docker(opts.docker_bin, &["exec", name, "sh", "-c", &diff_cmd]).await {
        Ok(p) => p,
        Err(e) => {
            prompt_error
                .get_or_insert_with(|| format!("git diff failed: {}", err_tail(&e.to_string())));
            String::new()
        }
    };

    // 5. Read the turn's cost from aura's ledger (best-effort → zeros).
    let (input_tokens, output_tokens, cost_micro_usd) = match read_cost(
        opts.docker_bin,
        name,
        &opts.session_id,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(instance = %instance.instance_id, error = %e, "cost read failed; zeros");
            (0, 0, 0)
        }
    };

    Ok(InstanceRun {
        instance_id: instance.instance_id.clone(),
        patch,
        input_tokens,
        output_tokens,
        cost_micro_usd,
        latency_ms,
        error: prompt_error.take(),
    })
}

async fn read_cost(docker_bin: &str, name: &str, session: &str) -> Result<(u64, u64, i64)> {
    let out = docker(
        docker_bin,
        &[
            "exec",
            "-e",
            "AURA_CONFIG_PATH=/installed-agent/aura.json",
            "-e",
            "RUST_LOG=off",
            name,
            CONTAINER_AURA_BIN,
            "cost",
            "show",
            "--session",
            session,
            "--json",
        ],
    )
    .await?;
    parse_cost_summary(&out)
}

// ---------------------------------------------------------------------------
// docker process helpers
// ---------------------------------------------------------------------------

/// Run `docker <args>`, capturing stdout; non-zero exit is an error.
async fn docker(bin: &str, args: &[&str]) -> Result<String> {
    docker_env(bin, args, &[]).await
}

/// `docker` with extra env vars set on the spawned docker process. Lets
/// `docker run -e NAME` (name only) copy a secret into the container by name —
/// the value rides in the docker process env, never in argv / `ps`.
async fn docker_env(bin: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn `{bin} {}`", args.first().copied().unwrap_or("")))?;
    if !out.status.success() {
        bail!(
            "`{bin} {}` failed ({}): {}",
            args.first().copied().unwrap_or(""),
            out.status,
            err_tail(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `docker` with text piped to the child's stdin (used to write the config).
async fn docker_with_stdin(bin: &str, args: &[&str], stdin_data: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn `{bin} {}`", args.first().copied().unwrap_or("")))?;
    child
        .stdin
        .take()
        .context("child stdin missing")?
        .write_all(stdin_data.as_bytes())
        .await
        .context("write to docker stdin")?;
    let out = child
        .wait_with_output()
        .await
        .context("wait docker stdin")?;
    if !out.status.success() {
        bail!(
            "`{bin}` (stdin) failed: {}",
            err_tail(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(())
}

/// `docker` capturing `(stdout, stderr, success)` with a hard timeout — used for
/// the agent turn, where a non-zero exit is noted but not fatal.
async fn docker_capture(
    bin: &str,
    args: &[&str],
    hard_timeout: Duration,
) -> Result<(String, String, bool)> {
    let fut = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(hard_timeout, fut).await {
        Ok(res) => {
            let out = res.context("run docker exec (aura prompt)")?;
            Ok((
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
                out.status.success(),
            ))
        }
        Err(_) => bail!("aura prompt exceeded the hard timeout ({hard_timeout:?})"),
    }
}

/// Detect an error object in `aura prompt --json` stdout. Returns `Some(reason)`
/// if the turn was rejected; `None` if it answered (or emitted no JSON).
fn prompt_output_error(stdout: &str) -> Option<String> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("response").is_some() {
            return None;
        }
        if let Some(e) = v.get("error").and_then(|x| x.as_str()) {
            return Some(format!("aura prompt turn error: {e}"));
        }
    }
    None
}

/// Parse `(input_tokens, output_tokens, cost_micro_usd)` from `aura cost show
/// --json`; tolerates leading log lines (parse from the first `{`).
fn parse_cost_summary(stdout: &str) -> Result<(u64, u64, i64)> {
    let start = stdout
        .find('{')
        .with_context(|| format!("no JSON in aura cost output: {}", err_tail(stdout)))?;
    let value: serde_json::Value = serde_json::from_str(stdout[start..].trim())
        .with_context(|| format!("parse aura cost JSON: {}", err_tail(stdout)))?;
    let summary = value
        .get("summary")
        .context("aura cost output has no `summary`")?;
    let u = |key: &str| {
        summary
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let cost = summary
        .get("cost_micro_usd")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    Ok((u("input_tokens"), u("output_tokens"), cost))
}

fn err_tail(s: &str) -> String {
    let trimmed = s.trim();
    let tail: String = trimmed
        .chars()
        .rev()
        .take(ERR_TAIL)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> AgentModel {
        AgentModel {
            provider: "deepseek".to_string(),
            model: "deepseek-v4-flash".to_string(),
            api_key_env: "API_KEY".to_string(),
            base_url: None,
        }
    }

    #[test]
    fn config_has_passthrough_and_isolated_workspace() {
        let cfg: serde_json::Value = serde_json::from_str(&render_agent_config(&model())).unwrap();
        assert_eq!(cfg["sandbox"]["mode"], "passthrough");
        // Workspace must be outside /testbed so aura state never pollutes the diff.
        assert_eq!(cfg["workspace"]["path"], CONTAINER_WORKSPACE);
        assert!(
            !cfg["workspace"]["path"]
                .as_str()
                .unwrap()
                .starts_with(TESTBED)
        );
        assert_eq!(cfg["default-llm"], "bench");
        assert_eq!(cfg["llm"][0]["provider"], "deepseek");
        assert!(cfg["llm"][0].get("base_url").is_none());
    }

    #[test]
    fn config_includes_base_url_when_set() {
        let mut m = model();
        m.base_url = Some("https://example.test/v1".to_string());
        let cfg: serde_json::Value = serde_json::from_str(&render_agent_config(&m)).unwrap();
        assert_eq!(cfg["llm"][0]["base_url"], "https://example.test/v1");
    }

    #[test]
    fn prompt_output_error_detects_error_object() {
        let out = "log line\n{\"session_id\":\"s\",\"error\":\"budget exceeded\"}\n";
        assert!(
            prompt_output_error(out)
                .unwrap()
                .contains("budget exceeded")
        );
    }

    #[test]
    fn prompt_output_ok_on_response() {
        let out = "{\"session_id\":\"s\",\"response\":\"done\"}";
        assert!(prompt_output_error(out).is_none());
    }

    #[test]
    fn prompt_output_tolerates_no_json() {
        assert!(prompt_output_error("plain text\n").is_none());
    }

    #[test]
    fn parses_cost_summary_with_leading_logs() {
        let out = "INFO some log\n{\"scope\":{},\"summary\":{\"cost_micro_usd\":1234,\"calls\":2,\"input_tokens\":40,\"output_tokens\":8}}";
        assert_eq!(parse_cost_summary(out).unwrap(), (40, 8, 1234));
    }

    #[test]
    fn cost_summary_errors_without_json() {
        assert!(parse_cost_summary("no json here").is_err());
    }
}
