//! `Bash` — execute a shell command via `sh -c` inside the OS sandbox.
//!
//! Every shell-needing command runs through bwrap (Linux) /
//! sandbox-exec (macOS) / docker, EXCEPT invocations of the local
//! `aura` CLI (any sub-command whose argv0 is
//! [`aura_workspace::paths::BIN_NAME`]). The sandbox masks the Aura
//! state dir (`~/.aura`/`$AURA_HOME`), so a sandboxed `aura …` call
//! can't see the parent gateway's config or session store — running
//! it sandboxed is broken by construction, so the agent's own CLI
//! gets the unsandboxed `sh -c` path directly.
//!
//! The sandbox runs in **permissive
//! filesystem** mode capped at `workspace_root + $HOME`: FHS roots
//! (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`,
//! `/run/systemd/resolve`) stay RO so installed binaries and
//! resolv.conf still work; credential vaults (`~/.ssh`, `~/.aws`,
//! `~/.gnupg`, `~/.gpg`, `~/.config/gh`, `~/.config/gcloud`,
//! `~/.docker`, `~/.kube`, and the Aura state dir under
//! `~/.aura`/`$AURA_HOME`) are masked with per-call empty `tmpfs`;
//! `/dev` is a fresh minimal devtmpfs (no host raw devices); network
//! is enabled. Anything outside `workspace_root + $HOME + FHS-RO` is
//! invisible inside the sandbox.
//!
//! File-content viewers (`cat`, `head`, `tail`, `sed`, `awk`, …) are
//! rejected at the tool layer to force the Read/Edit tools.
//!
//! Approval prompts only fire when the command tokens contain a
//! file-delete (`rm`, `rmdir`, `unlink`, `shred`, `srm`, `wipe`,
//! `find … -delete`) or a destructive `git` invocation (`clean -f`,
//! `reset --hard`, `branch -d`/`-D`/`--delete`, `tag -d`, `push -f`/
//! `--force`/`--force-with-lease`/`--delete`/`-d`, `stash drop`/
//! `clear`, `worktree remove`, `update-ref -d`, `filter-branch`,
//! `filter-repo`, `git rm`). If the sandbox itself refuses the
//! command (cwd outside the bound union, bwrap setup failure, …), a
//! separate prompt offers an unsandboxed retry. Environment
//! variables and `cd` changes do NOT persist across invocations.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use aura_workspace::{WorkspacePaths, absolutise};
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ApprovalDecision, ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_KIB: usize = MAX_OUTPUT_BYTES / 1024;

/// Description template. `{{max_output_kib}}`, `{{work_dir}}`, and
/// `{{platform}}` are filled in by [`BashTool::new`]; the work-dir and
/// platform live here (not in the agent's system prompt) so they're
/// adjacent to the tool that actually consumes them — the agent reads
/// the description right before composing a Bash call.
const DESCRIPTION_TEMPLATE: &str = r#"Execute a shell command in a fresh `sh -c` process. Environment changes and `cd` do not persist across invocations. Each of stdout and stderr is truncated at {{max_output_kib}} KiB.

IMPORTANT: Do NOT use Bash for tasks that have a dedicated tool:
- File-content viewers (`cat`, `head`, `tail`, `less`, `more`, `tac`) and file-driven text processors (`sed`, `awk`) are REJECTED at this layer when invoked as the leading command — use `Read` for content (`offset`/`limit` cover head/tail), `Edit` for in-place changes (safer than `sed -i`). Stream-mode `sed`/`awk` AFTER a pipe is fine (e.g. `git log | sed 's/.../.../'`) — only `sed <file>` / `awk <file>` is blocked.
- To write files use `Write` (not echo/cat with redirection)
- To search file names use `Glob` (not find/ls)
- To search file contents use `Grep` (not grep/rg)
- To download a file to disk (`.txt`, `.json`, `.csv`, archives, binaries, scripts, …) use Bash with `curl`/`wget` — WebFetch only returns rendered text into the conversation and never writes to disk.

SANDBOX: The shell runs with read+write access to the project workspace and `$HOME` (FHS roots `/usr`, `/bin`, `/etc`, … stay readable; nothing outside that union is visible — no full host-root bind). Credential vaults inside `$HOME` (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.gpg`, `~/.config/gh`, `~/.config/gcloud`, `~/.docker`, `~/.kube`, and the Aura state dir under `~/.aura`/`$AURA_HOME`) are masked with empty tmpfs and look empty inside the sandbox. Host raw devices stay unreachable (`/dev` is a minimal devtmpfs). Network is enabled.

APPROVAL: sandboxed commands run without prompting by default. The pre-execution approval gate only fires when the command tokens contain a file-delete (`rm`, `rmdir`, `unlink`, `shred`, `srm`, `wipe`, or `find … -delete`) or a destructive `git` operation (`clean -f`, `reset --hard`, `branch -d`/`-D`/`--delete`, `tag -d`, `push -f`/`--force`/`--force-with-lease`/`--delete`/`-d`, `stash drop`/`clear`, `worktree remove`, `update-ref -d`, `filter-branch`, `filter-repo`); a separate prompt also fires if the sandbox refuses the command and an unsandboxed retry is available.
Reserve Bash for system commands, git operations, build/test, and terminal tasks that require shell execution.

DEFAULT CWD: If `cwd` is omitted, Aura runs the command from the workspace work directory and exports `PWD` with the same value.

PATHS: Any directory or file argument inside the command (cd, ls, mkdir, rm, mv, cp, find, …) MUST be an absolute path. The optional `cwd` parameter MUST also be absolute when provided — relative values are rejected. Always quote file paths that contain spaces with double quotes (e.g. `cd "/path with spaces/file.txt"`).

WORK-DIR SCOPE: Bash may only touch files inside the workspace work directory ({{work_dir}}). Any absolute path argument that falls under the workspace root but outside `work/` (the sibling subtrees `profile/`, `config/`, `state/`, `logs/`, `skills/`, `.key/`) is rejected up front, and `cwd` is held to the same rule. Use the dedicated tools (Read, Edit, Write, …) when you genuinely need to read or modify those subtrees; everything else stays under {{work_dir}}.

BEFORE BROAD SCANS: Do not run `find`, `du`, recursive `ls`, or similar walks against unknown directories without first checking their size with a bounded probe (e.g. `ls -1 <dir> | wc -l`, or a shallow `find -maxdepth 2`). Large trees can hang the process.

PYTHON: `python`, `python3`, and `pip` are shimmed to `uv run python` / `uv pip` inside this shell. For one-file scripts with third-party deps, declare them via PEP 723 inline metadata (`# /// script` block) so `uv run --script my.py` resolves them per-call. The shims are shell functions scoped to the outer `sh -c` — `bash -c '…'` subshells, `/usr/bin/python`, and Python's own `subprocess` calls bypass them.

ENVIRONMENT:
- Working directory: {{work_dir}}
- Platform: {{platform}}"#;

pub struct BashTool {
    description: String,
    /// Absolute workspace root (`<workspace>`). Used together with
    /// [`Self::work_dir`] to reject path arguments that would touch
    /// non-`work/` subtrees (`profile/`, `config/`, `state/`, …).
    workspace_root: PathBuf,
    /// Absolute work directory (`<workspace>/work`). Sole writable area
    /// for Bash invocations.
    work_dir: PathBuf,
    /// Pre-rendered `export UV_*=…; ` chain prepended to every command so
    /// any `uv` invocation caches inside the workspace rather than
    /// `~/.cache/uv` / `~/.local/share/uv`. Non-uv processes inherit and
    /// ignore the variables — same loose-coupling rationale as
    /// [`inject_aura_env`].
    uv_env_prefix: String,
}

impl BashTool {
    pub fn new(workspace_paths: WorkspacePaths) -> Self {
        // Re-anchor at the absolutised root so the env-var values
        // rendered by `build_uv_env_exports` are absolute regardless
        // of whether `config.workspace.path` came in absolute — the
        // subshell inherits these as-is and tools running with a
        // different cwd must still resolve them.
        let paths = WorkspacePaths::new(absolutise(workspace_paths.root()));
        let workspace_root = paths.root().to_path_buf();
        let work_dir = paths.work_dir();
        Self {
            description: build_description(&work_dir, std::env::consts::OS),
            workspace_root,
            work_dir,
            uv_env_prefix: build_uv_env_exports(&paths),
        }
    }

    /// Prefix `command` with the workspace-scoped UV exports and the
    /// Aura-CLI env injection. Two callers (the sandboxed `execute` path
    /// and the unsandboxed retry below) compose the same `sh -c` body —
    /// keep the ordering in one place so a future reshuffle doesn't
    /// drift between them.
    fn wrap_command(&self, command: &str) -> String {
        let injected = inject_aura_env(command);
        let mut out = String::with_capacity(self.uv_env_prefix.len() + injected.len());
        out.push_str(&self.uv_env_prefix);
        out.push_str(&injected);
        out
    }
}

fn build_description(work_dir: &Path, platform: &str) -> String {
    DESCRIPTION_TEMPLATE
        .replace("{{max_output_kib}}", &MAX_OUTPUT_KIB.to_string())
        .replace("{{work_dir}}", &work_dir.display().to_string())
        .replace("{{platform}}", platform)
}

type UvEnvVar = (&'static str, fn(&WorkspacePaths) -> PathBuf);

/// `(env-var name, path accessor)` driving [`build_uv_env_exports`].
/// Single source for the var-name → dir-helper pairing so adding a
/// fifth `UV_…` knob is one row instead of four parallel call sites.
const UV_ENV_VARS: &[UvEnvVar] = &[
    ("UV_CACHE_DIR", WorkspacePaths::uv_cache_dir),
    ("UV_PYTHON_INSTALL_DIR", WorkspacePaths::uv_python_dir),
    ("UV_TOOL_DIR", WorkspacePaths::uv_tool_dir),
    ("UV_TOOL_BIN_DIR", WorkspacePaths::uv_tool_bin_dir),
];

/// sh function definitions that shim `python` / `python3` / `pip` to the
/// uv-managed equivalents. Functions only affect the immediate `sh -c`
/// body — they don't propagate into `bash -c '…'` subshells or absolute
/// `/usr/bin/python` invocations. That partial coverage is the trade-off
/// for not touching `$PATH` (no on-disk shim dir, no surprising
/// `which python` answer for tools that ask).
const UV_SHELL_SHIMS: &str = "python() { uv run python \"$@\"; }; \
                              python3() { uv run python \"$@\"; }; \
                              pip() { uv pip \"$@\"; }; ";

/// Trailing `; ` lets callers concatenate the command body directly
/// without a separator.
fn build_uv_env_exports(paths: &WorkspacePaths) -> String {
    let mut out = String::new();
    for (name, get) in UV_ENV_VARS {
        let path = get(paths);
        out.push_str("export ");
        out.push_str(name);
        out.push('=');
        out.push_str(&sh_quote(&path.to_string_lossy()));
        out.push_str("; ");
    }
    out.push_str(UV_SHELL_SHIMS);
    out
}

/// Default Python toolchain prefetched at boot. Pinning to a specific
/// minor keeps the resolved interpreter stable across boots (uv's
/// "latest stable" tracks releases). Bumping this in code is the right
/// mechanism — users who want a different version run `uv python
/// install <ver>` themselves and uv picks the newest installed.
const UV_PREWARM_PYTHON: &str = "3.13";

/// Best-effort: prefetch a default Python toolchain into the workspace
/// `UV_PYTHON_INSTALL_DIR` so the first agent-issued `python …` call
/// doesn't stall on a cold ~30 MB cpython download. Spawns a detached
/// tokio task; failure (uv not installed, network down, …) is logged
/// at WARN and otherwise swallowed — the agent loop must not depend on
/// this completing.
pub fn spawn_uv_python_prewarm(paths: &WorkspacePaths) {
    let env: Vec<(&'static str, PathBuf)> = UV_ENV_VARS
        .iter()
        .map(|(name, get)| (*name, get(paths)))
        .collect();
    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("uv");
        cmd.args(["python", "install", UV_PREWARM_PYTHON]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::piped());
        match cmd.output().await {
            Ok(out) if out.status.success() => {
                tracing::info!(
                    version = UV_PREWARM_PYTHON,
                    "uv python toolchain prefetched",
                );
            }
            Ok(out) => {
                tracing::warn!(
                    version = UV_PREWARM_PYTHON,
                    status = ?out.status,
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "uv python install exited non-zero",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "could not spawn `uv python install` (uv not on PATH?)",
                );
            }
        }
    });
}

#[cfg(any(test, feature = "test-support"))]
impl BashTool {
    /// Test-only constructor anchored at `/tmp` — production paths go
    /// through [`Self::new`] with the real workspace.
    pub fn for_test() -> Self {
        Self::new(WorkspacePaths::new("/tmp"))
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string", "description": "The shell command to run" },
                "timeout_ms": { "type": "integer", "minimum": 1, "description": "Per-command timeout in ms (falls back to the tool context timeout)" },
                "cwd":        { "type": "string", "description": "Working directory for the command" }
            },
            "required": ["command"]
        })
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        let cmd = params.get("command").and_then(|v| v.as_str())?;
        if contains_delete_command(cmd) {
            Some(
                "Destructive command: contains a file-delete operation \
                 (rm / rmdir / git clean / git reset --hard / …). The \
                 action is irreversible — review the command and the \
                 target paths carefully before approving."
                    .to_string(),
            )
        } else {
            None
        }
    }

    /// Progress preview = the command itself (whitespace-collapsed, length-
    /// capped via the shared helper), so the live `● Bash(<cmd>)` line is
    /// useful for *every* call — `call_label` above is a destructive-command
    /// warning, not a preview, so we don't inherit it here.
    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("command")
            .and_then(|v| v.as_str())
            .and_then(crate::progress::preview_arg)
    }

    fn max_timeout(&self) -> Duration {
        // Builds, test suites and migration scripts are all fair game
        // through Bash, so the trait default 30 s would clip anything
        // non-trivial. Cap the *outer* deadline at 10 min; per-call
        // `timeout_ms` (and the in-tool sandbox spawn) still tighten
        // further.
        Duration::from_secs(600)
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Sandboxed commands skip the pre-execution approval gate by
        // default. The OS sandbox constrains filesystem reach, and the
        // mid-execution prompt covers the unsandboxed retry path when
        // bwrap setup itself fails. The exception: file-delete tokens
        // (`rm`/`rmdir`/`find -delete`) and destructive `git` ops still
        // pop the prompt because a sandboxed delete inside the bound
        // surface is both legitimate AND irreversible.
        //
        // FileToolRedirect commands (`cat foo`, `sed -i …`) are rejected
        // before the sandbox spawn — pointless to ask either.
        params
            .get("command")
            .and_then(|v| v.as_str())
            .filter(|s| !is_file_tool_redirect(s) && contains_delete_command(s))
            .map(|s| {
                vec![ResourceAccess::ExecCommand {
                    command: s.to_string(),
                }]
            })
            .unwrap_or_default()
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        if let Some(dir) = &p.cwd {
            require_absolute(dir, "Bash", "cwd")?;
            require_within_work_dir(dir, &self.workspace_root, &self.work_dir, "cwd")?;
        }

        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.timeout);

        let command = p.command;
        require_command_paths_within_work_dir(&command, &self.workspace_root, &self.work_dir)?;
        let cwd_ref: Option<&Path> = Some(p.cwd.as_deref().unwrap_or(ctx.workspace_root.as_path()));

        if is_file_tool_redirect(&command) {
            let argv0 = first_token(&command).unwrap_or("?");
            return Err(ToolError::InvalidParams(format!(
                "Refusing to run `{argv0}` against a file via Bash. Use the right tool \
                 for the job:\n\
                 - Reading content: `Read` (with `offset`/`limit` for head- and \
                   tail-style slices).\n\
                 - In-place file edits: `Edit` (exact-string replacement, fail-fast \
                   on ambiguous matches; far safer than `sed -i`).\n\
                 - Stream filtering inside a pipeline: keep `{argv0}` AFTER a pipe so \
                   it consumes stdin instead of opening a file (e.g. \
                   `git log | sed 's/.../.../'` is fine — only `{argv0} <file>` is \
                   blocked).",
            )));
        }

        let aura_resolution = classify_aura_command(&command);
        if matches!(aura_resolution, AuraResolution::RequireAbsolutePath) {
            // The agent is clearly trying to invoke aura (basename
            // match) but used a bare/relative/wrong-absolute argv0.
            // Sandboxing would just fail opaquely on the masked
            // state dir; surface a precise instruction with the
            // correct absolute path so the agent can self-correct.
            let bin_display = AURA_BIN
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unknown>".into());
            return Err(ToolError::InvalidParams(format!(
                "Aura CLI invocations must use the absolute path of the gateway binary. \
                 Replace the argv0 with `{bin_display}` (e.g. \
                 `{bin_display} cost` instead of `aura cost`). \
                 Bare-name and relative-path invocations are rejected so the \
                 unsandboxed shell never resolves `aura` through `$PATH`."
            )));
        }

        let args = vec!["-c".into(), self.wrap_command(&command)];

        let out = if matches!(aura_resolution, AuraResolution::Bypass) {
            // The OS sandbox masks `~/.aura`/`$AURA_HOME`, so a
            // sandboxed `aura …` call can't reach the gateway's
            // config or session store — sandboxing aura's own CLI is
            // broken by construction. Argv0 is already an absolute
            // path canonicalising to the gateway binary, so the
            // unsandboxed `sh -c` execve's our binary directly with
            // no `$PATH` consultation.
            tokio::select! {
                _ = ctx.cancellation_token.cancelled() => {
                    return Err(ToolError::Execution("cancelled".into()));
                }
                res = run_unsandboxed("sh", &args, cwd_ref, timeout) => res?,
            }
        } else {
            let Some(sandbox) = ctx.sandbox.as_ref() else {
                return Err(ToolError::Execution(
                    "OS sandbox unavailable: install bwrap (Linux: `apt install bubblewrap`) \
                     or sandbox-exec (macOS, ships with the system) and restart aura"
                        .into(),
                ));
            };
            let attempt = tokio::select! {
                _ = ctx.cancellation_token.cancelled() => {
                    return Err(ToolError::Execution("cancelled".into()));
                }
                res = sandbox.spawn_command(Path::new("sh"), &args, cwd_ref, None, timeout) => res,
            };
            match attempt {
                Ok(out) => out,
                Err(sandbox_err) => {
                    // Sandbox infrastructure refused the command (cwd
                    // outside the bound union, bwrap setup failure,
                    // runner error, …). Offer the user a one-shot
                    // unsandboxed retry that surfaces the failure
                    // reason in the approval prompt.
                    self.prompt_and_run_unsandboxed_retry(
                        &command,
                        cwd_ref,
                        timeout,
                        ctx,
                        sandbox_err,
                    )
                    .await?
                }
            }
        };

        if out.timed_out {
            return Err(ToolError::Timeout(format!("Bash exceeded {timeout:?}")));
        }

        let stdout = truncate_utf8(&out.stdout, MAX_OUTPUT_BYTES);
        let stderr = truncate_utf8(&out.stderr, MAX_OUTPUT_BYTES);

        let mut result = json!({
            "exit_code": out.exit_code,
            "stdout": stdout,
            "stderr": stderr,
        });
        if let Some(hint) = interpret_exit(&command, out.exit_code) {
            result["return_code_interpretation"] = Value::String(hint.into());
        }

        Ok(ToolOutput::Json(result))
    }
}

/// Reject an absolute path that lives inside the workspace root but
/// outside the work directory. Bash invocations are scoped to the
/// `work/` subtree so the agent can't accidentally clobber the
/// gateway's own `config/`, `state/`, `profile/`, `logs/`, `skills/`,
/// or `.key/` subdirectories from a shell call. Paths that are not
/// absolute, or that fall entirely outside the workspace root (FHS
/// roots, `$HOME`, `/tmp`, …) are left to the OS sandbox.
fn require_within_work_dir(
    path: &Path,
    workspace_root: &Path,
    work_dir: &Path,
    label: &str,
) -> crate::Result<()> {
    if !path.is_absolute() {
        return Ok(());
    }
    if path.starts_with(workspace_root) && !path.starts_with(work_dir) {
        return Err(ToolError::InvalidParams(format!(
            "Bash {label} `{}` is inside the workspace but outside the work \
             directory. Only `{}` is writable for shell operations — move the \
             action under `{}/` or use Read/Edit/Write for the read-only \
             workspace subtrees (profile/, config/, state/, logs/, skills/, \
             .key/).",
            path.display(),
            work_dir.display(),
            work_dir.display(),
        )));
    }
    Ok(())
}

/// Walk every token of every sub-command and reject the first one
/// that resolves to an absolute path inside the workspace but outside
/// `work/`. The token loop reuses [`split_into_subcommands`] +
/// `shell_words::split` so quoted forms (`"<workspace>/profile"`,
/// `'<workspace>/state'`) are checked against their unquoted body.
/// Tokens that fail to unquote cleanly are skipped — they'd also slip
/// past the rest of the heuristic stack and the goal here is to catch
/// the obvious cases, not to be a full shell parser.
fn require_command_paths_within_work_dir(
    command: &str,
    workspace_root: &Path,
    work_dir: &Path,
) -> crate::Result<()> {
    for sub in split_into_subcommands(command) {
        for tok in sub {
            let Ok(words) = shell_words::split(tok) else {
                continue;
            };
            for word in words {
                let p = Path::new(&word);
                require_within_work_dir(p, workspace_root, work_dir, "command argument")?;
            }
        }
    }
    Ok(())
}

/// Map common diagnostic-tool exit codes to a human-readable hint so
/// the model doesn't treat `grep` / `diff` exit `1` as a failure.
/// Returns `None` for `0`, anything `>= 2`, and any command not in
/// the table.
///
/// `find` is intentionally absent: it exits `0` even when nothing
/// matches, so any `1` is a real traversal/permission/syntax error
/// — relabelling that as "no matches" would mask the failure.
fn interpret_exit(command: &str, exit_code: i32) -> Option<&'static str> {
    if exit_code != 1 {
        return None;
    }
    let argv0 = first_token(command)?;
    match argv0 {
        "grep" | "rg" | "ag" | "fgrep" | "egrep" => Some("no matches"),
        "diff" | "cmp" => Some("differences found"),
        _ => None,
    }
}

fn strip_path(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

const FILE_TOOL_REDIRECT_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "tac", "sed", "awk", "gawk", "mawk",
];

fn is_file_tool_redirect(command: &str) -> bool {
    first_token(command).is_some_and(|t| FILE_TOOL_REDIRECT_COMMANDS.contains(&t))
}

/// Prefix `command` with `export AURA_HELP_AGENT=1; export
/// AURA_CONFIG_PATH=…;` when the command contains the cargo bin name
/// (`aura_workspace::paths::BIN_NAME`). This gives the subshell two
/// things the agent would otherwise be missing:
///
/// 1. The extended-help inventory (hidden subcommands like `cost`,
///    `log`, `session`, `job`, `cron`, `config`). See
///    `aura_cli::cli::ENV_HELP_AGENT` for the reader contract.
/// 2. The same config file the running gateway is using. Reads
///    `AURA_CONFIG_PATH` from the parent process when set, falls
///    back to [`aura_workspace::paths::default_config_file`]
///    otherwise. The path is always resolved to an absolute form so
///    a relative debug-mode default (`./.aura/config/aura.json`)
///    keeps pointing at the right workspace even when the bash tool
///    spawns the child with a different cwd.
///
/// The substring match is intentionally loose: non-aura processes
/// inherit the variables and ignore them, so a false-positive
/// injection (e.g. `cd /data/aura && cargo build`) has no observable
/// effect. The win is that the agent can compose `aura …` commands
/// naturally — no per-call argv token, no LLM tool-shape change.
fn inject_aura_env(command: &str) -> String {
    let raw = std::env::var_os(aura_workspace::paths::ENV_CONFIG_PATH)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(aura_workspace::paths::default_config_file);
    // Syntactic absolutize — fast, doesn't touch the FS, doesn't
    // fail when the file is missing (which is the normal case in
    // fresh deployments before `aura setup` runs).
    let abs = std::path::absolute(&raw).unwrap_or(raw);
    inject_aura_env_with(command, abs.as_os_str())
}

/// Pure variant of [`inject_aura_env`] that takes an already-resolved
/// config path. Split out so tests don't have to mutate process env.
///
/// The "does this command invoke the CLI" check is a substring match
/// against `aura_workspace::paths::BIN_NAME` — that const is the
/// single source of truth for the cargo `[[bin]]` name, so renaming
/// the binary changes the trigger token automatically.
fn inject_aura_env_with(command: &str, config_path: &std::ffi::OsStr) -> String {
    if !command.contains(aura_workspace::paths::BIN_NAME) {
        return command.to_string();
    }
    format!(
        "export AURA_HELP_AGENT=1; export {}={}; {command}",
        aura_workspace::paths::ENV_CONFIG_PATH,
        sh_quote(&config_path.to_string_lossy()),
    )
}

/// POSIX single-quote a string for embedding in a `sh -c` command.
/// Always wraps in `'…'` (cheap and safe for paths with spaces,
/// `$`, backticks, etc.); inner single quotes become `'\''`, the
/// standard close-quote / escape / re-open-quote idiom.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// argv0 of the command string — the first whitespace-separated token,
/// path-stripped (`/usr/bin/ls` → `ls`). Does NOT skip wrappers (`xargs`,
/// `timeout`, `nohup`, …) and does NOT skip `KEY=VAL` env-var prefixes,
/// because callers (delete detection, FileToolRedirect classification,
/// exit-code interpretation) only care about the literal leading token.
fn first_token(command: &str) -> Option<&str> {
    let raw = command.split_whitespace().next()?;
    let bare = raw.trim_start_matches('\\');
    if bare.contains('=') && !bare.starts_with('=') {
        return None;
    }
    Some(strip_path(bare))
}

/// argv0s that delete files. Compared against the argv0 of each
/// sub-command (after env-prefix unwrapping) and against tokens that
/// follow a known wrapper or `find`'s `-exec`/`-execdir`/`-ok`/`-okdir`
/// primary. `mv` is intentionally absent — it relocates rather than
/// destroys; `dd`, `truncate`, and `>` redirection can also wipe data
/// but are too noisy to gate on. Path-prefixed forms (`/usr/bin/rm`)
/// are normalized through `strip_path` before matching. `-delete` is
/// the `find` primary, not an argv0, so it lives outside this list and
/// is checked as "appears anywhere in the argv".
const STANDALONE_DELETE_TOKENS: &[&str] = &["rm", "rmdir", "unlink", "shred", "srm", "wipe"];

/// Wrappers whose argv0 is itself benign but whose first non-flag
/// argument is the actual command being executed. We descend into the
/// wrapped command position rather than parsing each wrapper's flag
/// grammar perfectly — false positives (one extra approval prompt for
/// `xargs grep rm file` etc.) are tolerable; missing a real
/// `xargs rm` / `nohup rm` / `sudo rm` is not.
const WRAPPER_COMMANDS: &[&str] = &[
    "xargs", "nohup", "nice", "ionice", "timeout", "sudo", "doas", "env", "command", "exec",
];

/// Canonical absolute path of the running gateway binary, cached on
/// first read. Drives the aura sandbox-bypass match in
/// [`classify_aura_command`]: path-like argv0s (`/usr/local/bin/aura`,
/// `./target/debug/aura`) are compared against THIS path, not against
/// the literal string `"aura"`, so an unrelated binary that happens
/// to be named `aura` somewhere else on disk does NOT trigger the
/// bypass.
///
/// Falls back to the raw `current_exe()` path if `canonicalize` fails
/// (binary deleted post-exec, etc.); returns `None` only if
/// `current_exe()` itself errors, which is rare enough that we treat
/// it as "no aura CLI is locatable, sandbox every command".
static AURA_BIN: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let exe = std::env::current_exe().ok()?;
    Some(std::fs::canonicalize(&exe).unwrap_or(exe))
});

/// How [`BashTool::execute`] should treat a shell command relative to
/// the gateway's own `aura` CLI.
///
/// The sandbox masks the Aura state dir (`~/.aura`/`$AURA_HOME`), so a
/// sandboxed `aura …` can't reach the gateway's config or session
/// store. That makes the bypass logic worth getting right in three
/// directions:
///
/// - [`Bypass`](AuraResolution::Bypass): the command is a single,
///   safe aura invocation written with the **absolute path** of the
///   gateway binary. Run unsandboxed.
/// - [`RequireAbsolutePath`](AuraResolution::RequireAbsolutePath):
///   the command is clearly trying to invoke aura (its argv0's
///   `file_name` matches the gateway binary), but the caller used a
///   bare/relative/wrong-absolute path. Refuse with an error that
///   tells the caller the correct path — this is more useful than
///   sandboxing it (which would just fail opaquely on the masked
///   state dir).
/// - [`Sandbox`](AuraResolution::Sandbox): aura isn't the leading
///   sub-command (or doesn't appear at all), OR an unsafe-env shape
///   we don't want to bypass even with an absolute-path aura argv0.
enum AuraResolution {
    Bypass,
    RequireAbsolutePath,
    Sandbox,
}

fn classify_aura_command(command: &str) -> AuraResolution {
    let Some(bin) = AURA_BIN.as_deref() else {
        return AuraResolution::Sandbox;
    };
    classify_aura_command_with_bin(command, bin)
}

fn classify_aura_command_with_bin(command: &str, bin: &Path) -> AuraResolution {
    // Only the FIRST sub-command's argv0 matters: if the user opens
    // the command line with an absolute-path aura invocation, the
    // whole `sh -c` string runs unsandboxed (compound forms like
    // `aura … && cat /etc/passwd`, `aura … | jq`, `$(aura …)`
    // included). A non-aura leader keeps the sandbox.
    let subs = split_into_subcommands(command);
    let Some(tokens) = subs.first() else {
        return AuraResolution::Sandbox;
    };

    let mut unquoted: Vec<String> = Vec::with_capacity(tokens.len());
    for tok in tokens {
        match shell_words::split(tok) {
            Ok(mut words) if words.len() <= 1 => {
                unquoted.push(words.pop().unwrap_or_default());
            }
            _ => return AuraResolution::Sandbox,
        }
    }
    let mut i = 0;
    while let Some(tok) = unquoted.get(i) {
        if !is_env_assignment(tok) {
            break;
        }
        if !is_safe_aura_env_assignment(tok) {
            return AuraResolution::Sandbox;
        }
        i += 1;
    }
    let Some(argv0) = unquoted.get(i) else {
        return AuraResolution::Sandbox;
    };

    // "Looks like aura" — basename of argv0 matches the gateway
    // binary's basename. Catches bare `aura`, relative `./aura`,
    // and wrong absolute paths (`/opt/imposter/aura`) — every form
    // where the caller appears to be trying to spawn the aura CLI.
    let argv0_filename = Path::new(argv0).file_name();
    let bin_filename = bin.file_name();
    let looks_like_aura = match (argv0_filename, bin_filename) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if !looks_like_aura {
        return AuraResolution::Sandbox;
    }

    if argv0_is_absolute_aura_path(argv0, bin) {
        AuraResolution::Bypass
    } else {
        AuraResolution::RequireAbsolutePath
    }
}

/// Env assignments allowed as a prefix on an aura invocation without
/// forfeiting the sandbox bypass. The whitelist is intentionally narrow:
/// the `AURA_` family (gateway-owned config the CLI reads) and the two
/// `RUST_*` knobs the agent commonly uses to surface tracing. Anything
/// else — `PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*`,
/// `HOME`/`XDG_*`, locale vars, … — could redirect command resolution
/// or library loading and so must force the sandbox path.
fn is_safe_aura_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else {
        return false;
    };
    let key = &tok[..eq];
    key.starts_with("AURA_") || matches!(key, "RUST_LOG" | "RUST_BACKTRACE")
}

/// True when `argv0` is a literal absolute path that resolves to the
/// same FS object as `bin`. Bare names and relative paths are
/// rejected outright — the bypass requires the caller to have spelled
/// out the absolute path, so the unsandboxed shell's `execve` never
/// consults `$PATH` or the current working directory.
fn argv0_is_absolute_aura_path(argv0: &str, bin: &Path) -> bool {
    let argv0_path = Path::new(argv0);
    if !argv0_path.is_absolute() {
        return false;
    }
    // canonicalize chases symlinks and `..`; for fixture paths that
    // don't exist on disk it errors, in which case plain `Path`
    // equality is a safe fallback (no symlink expansion changes the
    // answer when neither path exists).
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(argv0_path) == canon(bin)
}

/// True when ANY sub-command of `command` performs a destructive
/// operation: a standalone delete argv0 (`rm`/`rmdir`/…), `find -delete`
/// (or `-exec rm`), or a known destructive `git` invocation (see
/// [`git_args_are_destructive`]). Used by `accessed_resources` to
/// decide whether the pre-execution approval gate should fire.
fn contains_delete_command(command: &str) -> bool {
    split_into_subcommands(command)
        .iter()
        .any(|sub| subcommand_is_destructive(sub))
}

fn subcommand_is_destructive(tokens: &[&str]) -> bool {
    // Run each raw token through shell_words so that the argv we
    // actually compare matches what `sh -c` would exec — quotes and
    // backslash escapes are removed (`'rm'`, `"rm"`, `r'm'`,
    // `/bin/'rm'`, `\rm`, `"git" reset --hard`, …). Without this step,
    // surrounding the argv0 with quotes silently bypasses the approval
    // gate. We fail closed (return true) on parse errors or any token
    // that yields more than one word: shell ambiguity should escalate
    // to a prompt, not slip through.
    let mut unquoted: Vec<String> = Vec::with_capacity(tokens.len());
    for tok in tokens {
        match shell_words::split(tok) {
            Ok(mut words) if words.len() <= 1 => {
                unquoted.push(words.pop().unwrap_or_default());
            }
            _ => return true,
        }
    }

    // `-delete` is a find primary — it can appear anywhere in the argv
    // and there's no need to identify a "command position" first.
    if unquoted.iter().any(|t| t == "-delete") {
        return true;
    }

    // `-exec`/`-execdir`/`-ok`/`-okdir` followed by a destructive
    // wrapped command. `find` semantics: the next token IS the wrapped
    // argv0; the rest of the argv until `;`/`+` is its argv. We don't
    // bother parsing the terminator — the argv0 check is enough.
    for idx in 0..unquoted.len() {
        if matches!(
            unquoted[idx].as_str(),
            "-exec" | "-execdir" | "-ok" | "-okdir"
        ) && let Some(wrapped) = unquoted.get(idx + 1)
            && argv0_is_destructive(wrapped, &unquoted[idx + 2..])
        {
            return true;
        }
    }

    // Identify the sub-command's argv0 — skip env-var assignments
    // (`KEY=val` prefix) so `LANG=C rm /foo` is recognised correctly.
    let mut i = 0;
    while i < unquoted.len() && is_env_assignment(&unquoted[i]) {
        i += 1;
    }
    let Some(argv0) = unquoted.get(i) else {
        return false;
    };
    let rest = &unquoted[i + 1..];

    if argv0_is_destructive(argv0, rest) {
        return true;
    }

    // Wrapper case (`xargs rm`, `nohup rm`, `sudo rm`, `LANG=C nice rm`,
    // …). We don't model each wrapper's flag grammar — any non-flag
    // token in `rest` is treated as a potential wrapped argv0. This
    // accepts a few false positives (`xargs grep rm file` would prompt
    // because `rm` could be the wrapped argv0) in exchange for catching
    // the common destructive patterns the previous flat-token detector
    // covered.
    if WRAPPER_COMMANDS.contains(&strip_path(argv0)) {
        for (rel_idx, tok) in rest.iter().enumerate() {
            if tok.starts_with('-') {
                continue;
            }
            if argv0_is_destructive(tok, &rest[rel_idx + 1..]) {
                return true;
            }
        }
    }

    false
}

fn argv0_is_destructive(argv0: &str, rest: &[String]) -> bool {
    let bare = strip_path(argv0);
    if STANDALONE_DELETE_TOKENS.contains(&bare) {
        return true;
    }
    if bare == "git" {
        let args: Vec<&str> = rest.iter().map(String::as_str).collect();
        return git_args_are_destructive(&args);
    }
    false
}

/// True if `tok` looks like a leading `KEY=value` env assignment.
/// Bash treats one or more such tokens at the start of a sub-command as
/// per-invocation env overrides, with the actual argv0 starting after
/// them. We use the conservative shape `[A-Za-z_][A-Za-z0-9_]*=…`.
fn is_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else {
        return false;
    };
    let key = &tok[..eq];
    if key.is_empty() {
        return false;
    }
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `args` is the slice after the literal `git` token. Skip leading
/// global options (which can take a separate value or embed one with
/// `=`), then match known destructive subcommand+flag combinations.
/// The match arms intentionally stay narrow — `git checkout .`, `git
/// restore`, `git stash pop`, and `git reset --soft` rewrite working
/// state but don't strictly delete, and gating them under approval is
/// noisy enough to outweigh the safety win.
fn git_args_are_destructive(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        if matches!(
            a,
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
        ) {
            i += 2;
            continue;
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        if matches!(a, "-p" | "-P") {
            i += 1;
            continue;
        }
        break;
    }
    let Some(&sub) = args.get(i) else {
        return false;
    };
    let rest = &args[i + 1..];
    match sub {
        "clean" => rest.iter().any(|a| is_git_clean_destructive_flag(a)),
        "reset" => rest.contains(&"--hard"),
        "branch" => rest
            .iter()
            .any(|a| matches!(*a, "-d" | "-D" | "--delete" | "--delete-merged")),
        "tag" => rest.iter().any(|a| matches!(*a, "-d" | "--delete")),
        "push" => rest.iter().any(|a| {
            matches!(
                *a,
                "-f" | "-d" | "--force" | "--force-with-lease" | "--delete"
            ) || a.starts_with("--force-with-lease=")
                || a.starts_with("--force-if-includes")
        }),
        "stash" => matches!(rest.first().copied(), Some("drop") | Some("clear")),
        "worktree" => matches!(rest.first().copied(), Some("remove")),
        "update-ref" => rest.iter().any(|a| matches!(*a, "-d" | "--delete")),
        "filter-branch" | "filter-repo" => true,
        // `git rm <pathspec>` removes files from the index (and the
        // working tree unless `--cached`). Always destructive.
        "rm" => true,
        _ => false,
    }
}

/// `git clean` only deletes when invoked with `-f`/`--force` (or a
/// combined short flag containing `f`, e.g. `-fd`, `-fdx`). `-n` /
/// `--dry-run` and `-i` / `--interactive` alone don't delete; we don't
/// model `-n` cancelling `-f` — a false-positive prompt for a dry-run
/// that also passes `-f` is acceptable.
fn is_git_clean_destructive_flag(arg: &str) -> bool {
    if arg == "-f" || arg == "--force" {
        return true;
    }
    if arg.starts_with("--") {
        return false;
    }
    if let Some(rest) = arg.strip_prefix('-') {
        return !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_alphabetic())
            && rest.contains('f');
    }
    false
}

/// Lightweight tokenizer that respects single- and double-quoted regions
/// and splits the command into one token-list per sub-command. A
/// sub-command boundary is `;`, `|`, `&`, `(`, `)`, or backtick (the
/// last splits even inside double quotes, since command substitution
/// fires there). Whitespace is a token boundary that stays inside the
/// current sub-command. Not a full shell parser — variable expansion,
/// `$(…)` content, and escapes inside single quotes are not
/// interpreted; the goal is "find argv0s per sub-command, well enough
/// to drive a security heuristic". The `$` of `$(…)` is left as a
/// stray token in the outer sub-command and the inner contents form
/// their own sub-command, which is enough for delete-detection.
fn split_into_subcommands(command: &str) -> Vec<Vec<&str>> {
    let bytes = command.as_bytes();
    let mut subs: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut start: Option<usize> = None;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_single && b == b'\\' && i + 1 < bytes.len() {
            if start.is_none() {
                start = Some(i);
            }
            i += 2;
            continue;
        }
        if !in_double && b == b'\'' {
            if start.is_none() {
                start = Some(i);
            }
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && b == b'"' {
            if start.is_none() {
                start = Some(i);
            }
            in_double = !in_double;
            i += 1;
            continue;
        }
        let outside_quotes = !in_single && !in_double;
        let is_subcmd_separator = (outside_quotes && matches!(b, b';' | b'|' | b'&' | b'(' | b')'))
            || (!in_single && b == b'`');
        let is_token_separator = outside_quotes && b.is_ascii_whitespace();
        if is_subcmd_separator {
            if let Some(s) = start.take() {
                current.push(&command[s..i]);
            }
            if !current.is_empty() {
                subs.push(std::mem::take(&mut current));
            }
        } else if is_token_separator {
            if let Some(s) = start.take() {
                current.push(&command[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
        i += 1;
    }
    if let Some(s) = start {
        current.push(&command[s..]);
    }
    if !current.is_empty() {
        subs.push(current);
    }
    subs
}

impl BashTool {
    async fn prompt_and_run_unsandboxed_retry(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout: Duration,
        ctx: &ToolContext,
        sandbox_err: ToolError,
    ) -> crate::Result<crate::SandboxedOutput> {
        let Some(approval) = ctx.approval.as_ref() else {
            return Err(ToolError::Execution(format!(
                "sandboxed run failed and no mid-execution approval handle is wired, \
                 cannot offer unsandboxed retry: {sandbox_err}"
            )));
        };
        let preview = format!(
            "Sandboxed `Bash` invocation failed.\n\
             Command : {command}\n\
             Reason  : {sandbox_err}\n\
             Approve to retry the SAME command WITHOUT the OS sandbox \
             (full shell, no workspace cwd guard, no resource limits)."
        );
        // Cache must be bypassed: a prior sandboxed approval for this
        // command does NOT cover an unsandboxed run, and we never persist
        // an "approve always" on this elevated path either.
        let decision = approval
            .request_uncached(
                "Bash",
                &ctx.session_id,
                &ctx.user,
                vec![ResourceAccess::ExecCommand {
                    command: command.to_string(),
                }],
                preview,
            )
            .await;
        match decision {
            ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => {
                let args = ["-c".to_string(), self.wrap_command(command)];
                tokio::select! {
                    _ = ctx.cancellation_token.cancelled() => {
                        Err(ToolError::Execution("cancelled".into()))
                    }
                    res = run_unsandboxed("sh", &args, cwd, timeout) => res,
                }
            }
            ApprovalDecision::Deny => Err(ToolError::Execution(format!(
                "sandboxed run failed and the unsandboxed retry was denied: {sandbox_err}"
            ))),
        }
    }
}

async fn run_unsandboxed(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> crate::Result<crate::SandboxedOutput> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        cmd.env("PWD", dir);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn `{program}`: {e}")))?;

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("child stdout pipe missing".into()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution("child stderr pipe missing".into()))?;

    // Cap unsandboxed stdout/stderr at MAX_OUTPUT_BYTES + slack so a
    // runaway producer (`ls -R /`, `du /`) can't OOM us. Dropping the
    // limited reader closes our end of the pipe; the child gets EPIPE on
    // its next write and exits early.
    let read_cap = (MAX_OUTPUT_BYTES + 1024) as u64;
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stdout_pipe.take(read_cap);
        let _ = limited.read_to_end(&mut buf).await;
        drop(limited);
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stderr_pipe.take(read_cap);
        let _ = limited.read_to_end(&mut buf).await;
        drop(limited);
        buf
    });

    let (timed_out, exit_code) = tokio::select! {
        wait = child.wait() => {
            (false, wait.ok().and_then(|s| s.code()).unwrap_or(-1))
        }
        _ = tokio::time::sleep(timeout) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            (true, -1)
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    Ok(crate::SandboxedOutput {
        exit_code,
        stdout,
        stderr,
        timed_out,
    })
}

fn truncate_utf8(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let total = bytes.len();
    let mut cut = max;
    while cut > 0 && (bytes[cut] & 0b1100_0000) == 0b1000_0000 {
        cut -= 1;
    }
    let elided = total - cut;
    let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    use std::fmt::Write as _;
    let _ = write!(s, "\n... [truncated {elided} bytes, total {total}] ...");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxedOutput;
    use crate::test_support::{FakeApprovalGate, FakeExecSandbox};
    use crate::{ApprovalHandle, ApprovedResource};
    use aura_model::{ChannelType, User};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn cfg(path: &str) -> std::ffi::OsString {
        std::ffi::OsString::from(path)
    }

    #[test]
    fn inject_aura_env_prefixes_aura_commands() {
        let c = cfg("/data/aura/aura.json");
        let out = inject_aura_env_with("aura cost show", c.as_os_str());
        assert!(
            out.starts_with("export AURA_HELP_AGENT=1; "),
            "expected AURA_HELP_AGENT export prefix, got: {out}"
        );
        assert!(
            out.contains("export AURA_CONFIG_PATH='/data/aura/aura.json'"),
            "expected config-path export, got: {out}"
        );
        assert!(out.ends_with("; aura cost show"));
    }

    #[test]
    fn inject_aura_env_quotes_config_path_with_spaces_and_quotes() {
        // Path with a space + an embedded single quote — the latter
        // is rare on disk but the escape path must still work.
        let c = cfg("/tmp/aura's space/aura.json");
        let out = inject_aura_env_with("aura doctor", c.as_os_str());
        assert!(
            out.contains("export AURA_CONFIG_PATH='/tmp/aura'\\''s space/aura.json'"),
            "expected POSIX-quoted path, got: {out}"
        );
    }

    #[test]
    fn progress_label_previews_every_command_while_call_label_only_warns() {
        let tool = BashTool::for_test();
        // The ⏺ Bash(...) progress preview is the command for any call…
        assert_eq!(
            tool.progress_label(&serde_json::json!({ "command": "ls -la" })),
            Some("ls -la".to_string()),
        );
        // …while call_label (the approval warning) stays None unless destructive.
        assert_eq!(
            tool.call_label(&serde_json::json!({ "command": "ls -la" })),
            None,
        );
        assert!(
            tool.call_label(&serde_json::json!({ "command": "rm -rf build" }))
                .is_some(),
        );
    }

    #[test]
    fn progress_label_collapses_whitespace_and_caps_length() {
        let tool = BashTool::for_test();
        assert_eq!(
            tool.progress_label(&serde_json::json!({ "command": "echo a\n   echo b" })),
            Some("echo a echo b".to_string()),
        );
        let label = tool
            .progress_label(&serde_json::json!({ "command": "x".repeat(200) }))
            .expect("long command yields a label");
        assert!(
            label.ends_with('…'),
            "over-long command is truncated: {label:?}"
        );
        assert_eq!(label.chars().count(), 61, "60 chars + ellipsis");
    }

    #[test]
    fn inject_aura_env_leaves_unrelated_commands_alone() {
        let c = cfg("/x/aura.json");
        assert_eq!(inject_aura_env_with("ls -la", c.as_os_str()), "ls -la");
        assert_eq!(
            inject_aura_env_with("git status", c.as_os_str()),
            "git status"
        );
    }

    #[test]
    fn inject_aura_env_triggers_inside_pipelines_and_chains() {
        let c = cfg("/x/aura.json");
        for cmd in [
            "aura status --live | jq .",
            "cd /tmp && aura cost show",
            "for i in 1 2; do aura job list; done",
        ] {
            let out = inject_aura_env_with(cmd, c.as_os_str());
            assert!(
                out.starts_with("export AURA_HELP_AGENT=1; "),
                "expected env prefix for {cmd:?}, got: {out}"
            );
            assert!(
                out.contains("export AURA_CONFIG_PATH="),
                "expected config-path export for {cmd:?}, got: {out}"
            );
        }
    }

    #[test]
    fn inject_aura_env_falls_back_to_default_config_when_env_unset() {
        // Exercises the public wrapper rather than the pure helper —
        // verifies that an unset `AURA_CONFIG_PATH` still produces an
        // export pointing at the workspace default. We can't safely
        // mutate process env in a parallel test, so we settle for
        // asserting the export is present + absolute.
        // SAFETY: this test runs in the bash unit-test module; tokio
        // is not initialized, no concurrent reader is observing the
        // var while we mutate it.
        unsafe {
            std::env::remove_var(aura_workspace::paths::ENV_CONFIG_PATH);
        }
        let out = inject_aura_env("aura status");
        assert!(
            out.contains("export AURA_CONFIG_PATH='"),
            "default config should still be exported, got: {out}"
        );
        // The default workspace root is absolute in release and
        // resolves to absolute via `std::path::absolute` in debug —
        // either way the exported path starts with `/`.
        let after_eq = out
            .split("AURA_CONFIG_PATH='")
            .nth(1)
            .expect("export present");
        assert!(
            after_eq.starts_with('/'),
            "exported path should be absolute, got: {after_eq}"
        );
    }

    #[test]
    fn build_uv_env_exports_points_at_workspace_subdirs() {
        let paths = WorkspacePaths::new("/var/aura");
        let prefix = build_uv_env_exports(&paths);
        assert!(
            prefix.contains("export UV_CACHE_DIR='/var/aura/work/.uv/cache'"),
            "UV_CACHE_DIR missing or wrong, got: {prefix}",
        );
        assert!(
            prefix.contains("export UV_PYTHON_INSTALL_DIR='/var/aura/work/.uv/python'"),
            "UV_PYTHON_INSTALL_DIR missing or wrong, got: {prefix}",
        );
        assert!(
            prefix.contains("export UV_TOOL_DIR='/var/aura/work/.uv/tools'"),
            "UV_TOOL_DIR missing or wrong, got: {prefix}",
        );
        assert!(
            prefix.contains("export UV_TOOL_BIN_DIR='/var/aura/work/.uv/bin'"),
            "UV_TOOL_BIN_DIR missing or wrong, got: {prefix}",
        );
        assert!(
            prefix.contains("python() { uv run python \"$@\"; }"),
            "python shim function missing, got: {prefix}",
        );
        assert!(
            prefix.contains("python3() { uv run python \"$@\"; }"),
            "python3 shim function missing, got: {prefix}",
        );
        assert!(
            prefix.contains("pip() { uv pip \"$@\"; }"),
            "pip shim function missing, got: {prefix}",
        );
        assert!(
            prefix.ends_with("; "),
            "prefix must terminate with `; ` so the user command appends cleanly, got: {prefix}",
        );
    }

    #[test]
    fn build_uv_env_exports_quotes_paths_with_special_chars() {
        let paths = WorkspacePaths::new("/tmp/aura's space");
        let prefix = build_uv_env_exports(&paths);
        assert!(
            prefix.contains("export UV_CACHE_DIR='/tmp/aura'\\''s space/work/.uv/cache'"),
            "UV_CACHE_DIR must be POSIX-quoted, got: {prefix}",
        );
    }

    fn classify(command: &str, bin: &Path) -> AuraResolution {
        classify_aura_command_with_bin(command, bin)
    }

    #[test]
    fn classify_aura_bypasses_only_absolute_canonical_path() {
        // Core invariant: ONLY a literal absolute-path argv0 that
        // canonicalises to the gateway binary bypasses the sandbox.
        let bin = Path::new("/usr/local/bin/aura");
        assert!(matches!(
            classify("/usr/local/bin/aura cost", bin),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify("/usr/local/bin/aura", bin),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify("/usr/local/bin/aura status --live", bin),
            AuraResolution::Bypass
        ));
        // Quoted forms still bypass — shell_words::split strips the
        // wrapping single quotes before the canonical compare.
        assert!(matches!(
            classify("'/usr/local/bin/aura' cost", bin),
            AuraResolution::Bypass
        ));
        // Whitelisted env prefixes preserve the bypass.
        assert!(matches!(
            classify("AURA_LOG=trace /usr/local/bin/aura log", bin),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify(
                "AURA_LOG=trace AURA_HOME=/x /usr/local/bin/aura status",
                bin
            ),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify("RUST_LOG=debug /usr/local/bin/aura status", bin),
            AuraResolution::Bypass
        ));
    }

    #[test]
    fn classify_aura_demands_absolute_path_for_bare_or_relative_argv0() {
        // The user-asked behaviour: anything that LOOKS like an aura
        // invocation (basename match) but isn't spelled out as an
        // absolute path must error rather than silently sandbox.
        // BashTool::execute surfaces this as `InvalidParams` so the
        // agent self-corrects to the canonical absolute path.
        let bin = Path::new("/usr/local/bin/aura");
        assert!(matches!(
            classify("aura", bin),
            AuraResolution::RequireAbsolutePath
        ));
        assert!(matches!(
            classify("aura cost", bin),
            AuraResolution::RequireAbsolutePath
        ));
        assert!(matches!(
            classify("aura status --live", bin),
            AuraResolution::RequireAbsolutePath
        ));
        // Relative path forms still look like aura but aren't
        // absolute → require absolute path.
        assert!(matches!(
            classify("./aura cost", bin),
            AuraResolution::RequireAbsolutePath
        ));
        // Quoted bare name normalises to bare `aura`.
        assert!(matches!(
            classify("'aura' cost", bin),
            AuraResolution::RequireAbsolutePath
        ));
        // Whitelisted env + bare argv0 — still require absolute
        // path; safe env doesn't excuse the missing path.
        assert!(matches!(
            classify("AURA_LOG=trace aura log", bin),
            AuraResolution::RequireAbsolutePath
        ));
    }

    #[test]
    fn classify_aura_demands_absolute_path_for_wrong_absolute_path() {
        // An absolute path whose `file_name` matches but which doesn't
        // resolve to our gateway binary is also a misuse: the caller
        // is trying to spawn "aura", but the path points elsewhere.
        // Surface the corrective error rather than sandboxing the
        // imposter binary.
        let bin = Path::new("/usr/local/bin/aura");
        assert!(matches!(
            classify("/opt/imposter/aura --steal", bin),
            AuraResolution::RequireAbsolutePath
        ));
    }

    #[test]
    fn classify_aura_sandbox_for_non_aura_commands() {
        let bin = Path::new("/usr/local/bin/aura");
        assert!(matches!(classify("ls -la", bin), AuraResolution::Sandbox));
        assert!(matches!(
            classify("git status", bin),
            AuraResolution::Sandbox
        ));
        // Different basename → not an aura attempt at all.
        assert!(matches!(
            classify("aurality cost", bin),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("echo aura", bin),
            AuraResolution::Sandbox
        ));
        // Wrappers — argv0 is the wrapper, not `aura`, so we don't
        // treat this as an aura attempt. (The wrapped sandbox call
        // will fail because the state dir is masked; the agent
        // learns to drop the wrapper.)
        assert!(matches!(
            classify("nohup aura cost", bin),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("xargs aura", bin),
            AuraResolution::Sandbox
        ));
    }

    #[test]
    fn classify_aura_bypasses_compound_commands_led_by_aura() {
        // Only the FIRST sub-command's argv0 is inspected — when it
        // is the absolute-path aura binary, the entire `sh -c`
        // string runs unsandboxed, trailing segments included.
        let bin = Path::new("/usr/local/bin/aura");
        assert!(matches!(
            classify("/usr/local/bin/aura status; cat /home/u/.ssh/id_rsa", bin),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify("/usr/local/bin/aura status && curl evil", bin),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify("/usr/local/bin/aura status || true", bin),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify("/usr/local/bin/aura cost | head", bin),
            AuraResolution::Bypass
        ));
        assert!(matches!(
            classify("/usr/local/bin/aura & disown", bin),
            AuraResolution::Bypass
        ));
    }

    #[test]
    fn classify_aura_sandbox_when_aura_not_leading() {
        // Non-aura leaders keep the sandbox even when aura appears
        // later in the pipeline — the leader's argv0 is what drives
        // the classification.
        let bin = Path::new("/usr/local/bin/aura");
        assert!(matches!(
            classify("echo $(/usr/local/bin/aura status)", bin),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("echo `/usr/local/bin/aura status`", bin),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("cd /tmp && /usr/local/bin/aura status", bin),
            AuraResolution::Sandbox
        ));
    }

    #[test]
    fn classify_aura_sandbox_for_unsafe_env_prefixes() {
        // Codex P1 fix: env vars outside the whitelist could subvert
        // the aura process even with an absolute-path argv0
        // (`LD_PRELOAD` injection, `HOME` redirection, etc.). Force
        // the sandbox path rather than raising the absolute-path
        // error — fixing the path alone wouldn't make the command
        // safe.
        let bin = Path::new("/usr/local/bin/aura");
        assert!(matches!(
            classify(
                "PATH=/tmp/malicious:/usr/bin /usr/local/bin/aura status",
                bin
            ),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("LD_PRELOAD=/tmp/evil.so /usr/local/bin/aura status", bin),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("LD_LIBRARY_PATH=/tmp/evil /usr/local/bin/aura status", bin),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify(
                "DYLD_INSERT_LIBRARIES=/tmp/evil.dylib /usr/local/bin/aura status",
                bin
            ),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("HOME=/tmp /usr/local/bin/aura status", bin),
            AuraResolution::Sandbox
        ));
        // Quote-stripped form must reach the same conclusion.
        assert!(matches!(
            classify("'PATH=/tmp' /usr/local/bin/aura status", bin),
            AuraResolution::Sandbox
        ));
        // Mixed prefix: an unsafe key anywhere in the chain kills
        // the bypass.
        assert!(matches!(
            classify("AURA_LOG=trace PATH=/tmp /usr/local/bin/aura status", bin),
            AuraResolution::Sandbox
        ));
    }

    #[test]
    fn classify_aura_uses_bin_file_name() {
        // If the gateway was installed under a different file_name
        // (`aura2`), `aura` is no longer an aura attempt — it's just
        // an unrelated command → sandbox. The new basename drives
        // both the `looks like aura` check and the canonical-path
        // bypass.
        let bin = Path::new("/usr/local/bin/aura2");
        assert!(matches!(
            classify("aura cost", bin),
            AuraResolution::Sandbox
        ));
        assert!(matches!(
            classify("aura2 cost", bin),
            AuraResolution::RequireAbsolutePath
        ));
        assert!(matches!(
            classify("/usr/local/bin/aura2 cost", bin),
            AuraResolution::Bypass
        ));
    }

    fn ctx_with(sandbox: Option<Arc<dyn crate::ExecSandbox>>) -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox,
            approval: None,
            notifier: None,
            events: crate::noop_event_sink(),
            llm: None,
        }
    }

    fn ctx_with_approval(
        sandbox: Option<Arc<dyn crate::ExecSandbox>>,
        gate: Arc<FakeApprovalGate>,
    ) -> ToolContext {
        let mut ctx = ctx_with(sandbox);
        let cache: Arc<Mutex<Vec<ApprovedResource>>> = Arc::new(Mutex::new(Vec::new()));
        ctx.approval = Some(ApprovalHandle::new(gate, cache));
        ctx
    }

    fn fake_with_response(
        out: SandboxedOutput,
    ) -> (Arc<FakeExecSandbox>, Arc<dyn crate::ExecSandbox>) {
        let fake = Arc::new(FakeExecSandbox::new());
        fake.set_response(out);
        let dyn_handle: Arc<dyn crate::ExecSandbox> = fake.clone();
        (fake, dyn_handle)
    }

    /// `ExecSandbox` whose `spawn_command` always returns the configured
    /// `Err` — needed to exercise the sandbox-failure → unsandboxed-retry
    /// path. `FakeExecSandbox` only models the success side.
    struct FailingExecSandbox {
        message: String,
    }

    #[async_trait::async_trait]
    impl crate::ExecSandbox for FailingExecSandbox {
        async fn spawn_command(
            &self,
            _program: &Path,
            _args: &[String],
            _cwd: Option<&Path>,
            _stdin: Option<&[u8]>,
            _timeout: Duration,
        ) -> crate::Result<SandboxedOutput> {
            Err(ToolError::Execution(self.message.clone()))
        }
    }

    #[tokio::test]
    async fn refuses_when_sandbox_missing() {
        let err = BashTool::for_test()
            .execute(json!({ "command": "echo hi" }), &ctx_with(None))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("OS sandbox unavailable")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn routes_command_through_sandbox() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool::for_test()
            .execute(json!({ "command": "echo hello" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("hello"));

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, std::path::PathBuf::from("sh"));
        assert_eq!(calls[0].args[0], "-c");
        // Tighter than `contains` — locks the prefix to the head so a
        // future reorder can't accidentally drop it and still pass.
        assert!(
            calls[0].args[1].starts_with("export UV_CACHE_DIR="),
            "uv env prefix must lead the sh -c body, got: {}",
            calls[0].args[1],
        );
        assert!(
            calls[0].args[1].ends_with("echo hello"),
            "command body should land at the tail of the sh -c arg, got: {}",
            calls[0].args[1],
        );
    }

    #[tokio::test]
    async fn execute_rejects_bare_aura_with_absolute_path_error() {
        // When the agent invokes the gateway binary by bare name
        // (basename match, no leading `/`), `execute` must refuse
        // with an `InvalidParams` error that names the correct
        // absolute path. Sandboxing it would just fail opaquely on
        // the masked state dir; the explicit error trains the agent
        // to use the canonical path.
        let exe = std::env::current_exe().expect("current_exe in test");
        let exe_name = exe
            .file_name()
            .expect("test binary has a file_name")
            .to_string_lossy()
            .to_string();
        let exe_canon = std::fs::canonicalize(&exe).unwrap_or_else(|_| exe.clone());

        let err = BashTool::for_test()
            .execute(
                json!({ "command": format!("{exe_name} --probe") }),
                &ctx_with(None),
            )
            .await
            .unwrap_err();
        let ToolError::InvalidParams(msg) = err else {
            panic!("expected InvalidParams, got {err:?}");
        };
        assert!(
            msg.contains("absolute path") && msg.contains(&exe_canon.display().to_string()),
            "error must teach the canonical absolute path: {msg}"
        );
    }

    #[tokio::test]
    async fn aura_invocations_bypass_the_sandbox() {
        // `aura …` commands must NOT consult the sandbox: the sandbox
        // masks `~/.aura`/`$AURA_HOME`, so a sandboxed aura process
        // can't see the gateway's config or session store. Two
        // assertions:
        //   1. The fake sandbox is never invoked.
        //   2. Even without ANY sandbox configured, the command still
        //      runs (no "OS sandbox unavailable" error).
        //
        // The match is keyed off the running binary's `current_exe()`
        // — under `cargo test` that's the test harness binary (e.g.
        // `aura_tools-XXXX`), NOT `aura`. So we drive the bypass with
        // the absolute test-binary path; the underlying `sh -c` will
        // try to run the test binary with a non-existent arg, which
        // exits quickly without re-entering test discovery.
        let exe = std::env::current_exe().expect("current_exe in test");
        let exe_path = exe.to_string_lossy().to_string();

        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let cmd = format!("{exe_path} --aura-bypass-probe-nonexistent-arg");
        let out = BashTool::for_test()
            .execute(
                json!({ "command": cmd, "timeout_ms": 5000 }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .expect("aura command must run unsandboxed");
        let ToolOutput::Json(_) = out else { panic!() };
        assert!(
            fake.calls().is_empty(),
            "aura invocations must skip the sandbox: {:?}",
            fake.calls()
        );

        // And the bypass works even when no sandbox is installed.
        let cmd = format!("{exe_path} --aura-bypass-probe-nonexistent-arg");
        BashTool::for_test()
            .execute(
                json!({ "command": cmd, "timeout_ms": 5000 }),
                &ctx_with(None),
            )
            .await
            .expect("aura command must run even without a sandbox");
    }

    #[tokio::test]
    async fn metadata_commands_now_route_through_sandbox() {
        // Regression for the refactor that retired the unsandboxed
        // metadata fast lane: `pwd`, `ls`, `stat`, … now go through the
        // OS sandbox like every other shell command. The sandbox
        // permissive policy makes `/etc`, `$HOME`, etc. visible, so the
        // common metadata calls keep working — they're just no longer
        // a separate code path with separate error semantics.
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: b"/tmp\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        BashTool::for_test()
            .execute(json!({ "command": "pwd" }), &ctx_with(Some(sandbox)))
            .await
            .expect("pwd must run");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "pwd must consult the sandbox now");
        assert_eq!(calls[0].args[0], "-c");
        assert!(
            calls[0].args[1].starts_with("export UV_CACHE_DIR="),
            "uv env prefix must lead the sh -c body, got: {}",
            calls[0].args[1],
        );
        assert!(
            calls[0].args[1].ends_with("pwd"),
            "command body should land at the tail of the sh -c arg, got: {}",
            calls[0].args[1],
        );
    }

    #[tokio::test]
    async fn surfaces_non_zero_exit_from_sandbox() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 7,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool::for_test()
            .execute(json!({ "command": "exit 7" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 7);
    }

    #[tokio::test]
    async fn timeout_flag_yields_timeout_error() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: -1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: true,
        });
        let err = BashTool::for_test()
            .execute(
                json!({ "command": "sleep 5", "timeout_ms": 50 }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn rejects_relative_cwd_before_sandbox_dispatch() {
        let err = BashTool::for_test()
            .execute(
                json!({ "command": "echo hi", "cwd": "relative/path" }),
                &ctx_with(None),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_cwd_inside_workspace_but_outside_work_dir() {
        // Test workspace anchors at `/tmp` (so work dir = `/tmp/work`).
        // A cwd pointing at `/tmp/profile` is inside the workspace but
        // outside `work/` — must be rejected before any sandbox dispatch
        // with an error that names both the offending path and the
        // mandatory work dir.
        let err = BashTool::for_test()
            .execute(
                json!({ "command": "echo hi", "cwd": "/tmp/profile" }),
                &ctx_with(None),
            )
            .await
            .unwrap_err();
        let ToolError::InvalidParams(msg) = err else {
            panic!("expected InvalidParams, got {err:?}");
        };
        assert!(
            msg.contains("/tmp/profile") && msg.contains("/tmp/work"),
            "error must name the offending path and the work dir: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_command_argument_inside_workspace_but_outside_work_dir() {
        // A path token buried in the command body (`ls /tmp/state/db`)
        // is just as out-of-scope as a `cwd` outside `work/`. The
        // validator must walk every sub-command token and refuse the
        // first hit before the sandbox runs.
        let err = BashTool::for_test()
            .execute(json!({ "command": "ls /tmp/state/db" }), &ctx_with(None))
            .await
            .unwrap_err();
        let ToolError::InvalidParams(msg) = err else {
            panic!("expected InvalidParams, got {err:?}");
        };
        assert!(
            msg.contains("/tmp/state/db"),
            "error must name the offending command argument: {msg}"
        );
    }

    #[tokio::test]
    async fn accepts_paths_under_work_dir_and_outside_workspace() {
        // `/tmp/work/foo` (inside work) and `/etc/hosts` (outside the
        // workspace entirely — FHS roots are the sandbox's problem,
        // not the work-dir guard's) must both pass through to the
        // sandbox without an `InvalidParams` rejection.
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        BashTool::for_test()
            .execute(
                json!({ "command": "ls /tmp/work/foo /etc/hosts", "cwd": "/tmp/work" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .expect("paths under work_dir and outside workspace must be accepted");
    }

    #[test]
    fn require_within_work_dir_only_flags_workspace_non_work() {
        let ws = Path::new("/tmp");
        let work = Path::new("/tmp/work");

        // Inside workspace, outside work — reject.
        assert!(require_within_work_dir(Path::new("/tmp/profile"), ws, work, "cwd").is_err());
        assert!(require_within_work_dir(Path::new("/tmp/state/x"), ws, work, "cwd").is_err());
        assert!(require_within_work_dir(Path::new("/tmp/config"), ws, work, "cwd").is_err());

        // Inside work — accept.
        assert!(require_within_work_dir(Path::new("/tmp/work"), ws, work, "cwd").is_ok());
        assert!(require_within_work_dir(Path::new("/tmp/work/a/b"), ws, work, "cwd").is_ok());

        // Outside workspace entirely — accept (sandbox's domain, not ours).
        assert!(require_within_work_dir(Path::new("/etc/hosts"), ws, work, "cwd").is_ok());
        assert!(require_within_work_dir(Path::new("/usr/bin/ls"), ws, work, "cwd").is_ok());

        // Relative paths — skip; absolute is required separately.
        assert!(require_within_work_dir(Path::new("relative/x"), ws, work, "cwd").is_ok());
    }

    #[test]
    fn require_command_paths_within_work_dir_walks_subcommands() {
        let ws = Path::new("/tmp");
        let work = Path::new("/tmp/work");

        // Clean command — no offending paths.
        assert!(
            require_command_paths_within_work_dir("ls /tmp/work && cat /etc/hosts", ws, work)
                .is_ok()
        );

        // Quoted path inside the workspace but outside work — caught
        // after `shell_words::split` unquotes the token.
        let err = require_command_paths_within_work_dir(r#"ls "/tmp/profile/foo""#, ws, work)
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(ref m) if m.contains("/tmp/profile/foo")));

        // Path hidden behind a pipeline still gets walked.
        let err = require_command_paths_within_work_dir("git status | tee /tmp/logs/out", ws, work)
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(ref m) if m.contains("/tmp/logs/out")));
    }

    #[tokio::test]
    async fn grep_exit_one_reports_no_matches() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool::for_test()
            .execute(
                json!({ "command": "grep foo bar.txt" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["return_code_interpretation"], "no matches");
    }

    #[tokio::test]
    async fn grep_exit_two_has_no_interpretation() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 2,
            stdout: Vec::new(),
            stderr: b"grep: bar.txt: No such file\n".to_vec(),
            timed_out: false,
        });
        let out = BashTool::for_test()
            .execute(
                json!({ "command": "grep foo bar.txt" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert!(v.get("return_code_interpretation").is_none());
    }

    #[tokio::test]
    async fn non_diagnostic_exit_one_has_no_interpretation() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool::for_test()
            .execute(json!({ "command": "false" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert!(v.get("return_code_interpretation").is_none());
    }

    #[tokio::test]
    async fn diff_exit_one_reports_differences_found() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: b"< a\n> b\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool::for_test()
            .execute(
                json!({ "command": "diff a.txt b.txt" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["return_code_interpretation"], "differences found");
    }

    #[tokio::test]
    async fn sandbox_err_with_approval_approve_falls_back_to_unsandboxed() {
        let sandbox: Arc<dyn crate::ExecSandbox> = Arc::new(FailingExecSandbox {
            message: "cwd `/etc` outside workspace".into(),
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        let out = BashTool::for_test()
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .expect("approved unsandboxed retry must succeed");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap_or("").contains("hi"));

        // The approval prompt fired once, with the failure reason and
        // the original command both surfaced in the params preview.
        let reqs = gate.requests();
        assert_eq!(reqs.len(), 1, "expected one approval prompt: {reqs:?}");
        assert!(
            reqs[0]
                .params_preview
                .contains("cwd `/etc` outside workspace")
                && reqs[0].params_preview.contains("echo hi")
                && reqs[0].params_preview.contains("WITHOUT the OS sandbox"),
            "preview missing required context: {}",
            reqs[0].params_preview
        );
    }

    #[tokio::test]
    async fn sandbox_err_with_approval_deny_surfaces_original_error() {
        let sandbox: Arc<dyn crate::ExecSandbox> = Arc::new(FailingExecSandbox {
            message: "bwrap setup failure".into(),
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Deny));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        let err = BashTool::for_test()
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .unwrap_err();
        let ToolError::Execution(msg) = err else {
            panic!("expected Execution error, got: {err:?}");
        };
        assert!(
            msg.contains("bwrap setup failure") && msg.contains("denied"),
            "deny should annotate the original sandbox error: {msg}"
        );
        assert_eq!(gate.requests().len(), 1, "deny path still prompts once");
    }

    #[tokio::test]
    async fn sandbox_err_without_approval_handle_returns_explanatory_error() {
        let sandbox: Arc<dyn crate::ExecSandbox> = Arc::new(FailingExecSandbox {
            message: "bwrap setup failure".into(),
        });
        let ctx = ctx_with(Some(sandbox));

        let err = BashTool::for_test()
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .unwrap_err();
        let ToolError::Execution(msg) = err else {
            panic!("expected Execution error, got: {err:?}");
        };
        assert!(
            msg.contains("no mid-execution approval handle") && msg.contains("bwrap setup failure"),
            "error must explain why retry wasn't offered AND keep original reason: {msg}"
        );
    }

    #[tokio::test]
    async fn sandbox_ok_does_not_prompt_for_unsandboxed_retry() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: b"command failed\n".to_vec(),
            timed_out: false,
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        // Non-zero exit is the command's own failure, not a sandbox
        // infrastructure failure. We must NOT auto-prompt for an
        // unsandboxed retry — that would be noise on every failed test.
        let out = BashTool::for_test()
            .execute(json!({ "command": "false" }), &ctx)
            .await
            .expect("non-zero exit returns Ok");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 1);
        assert!(
            gate.requests().is_empty(),
            "no approval prompt for non-zero exit: {:?}",
            gate.requests()
        );
    }

    #[tokio::test]
    async fn network_failure_in_sandbox_does_not_auto_escalate() {
        // Pre-refactor we tried to detect "sandbox blocked the network"
        // by stderr-pattern matching and prompted for an unsandboxed
        // retry. Now that the Bash sandbox runs with NetworkPolicy::All,
        // a "could not resolve host" stderr is a real network failure
        // (DNS broken, host unreachable, …), and an unsandboxed retry
        // wouldn't help. Surface the failure as-is.
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 6,
            stdout: Vec::new(),
            stderr: b"curl: (6) Could not resolve host: example.com\n".to_vec(),
            timed_out: false,
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        let out = BashTool::for_test()
            .execute(json!({ "command": "curl https://example.com" }), &ctx)
            .await
            .expect("network failure surfaces as ordinary non-zero exit");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 6);
        assert!(
            v["stderr"]
                .as_str()
                .unwrap_or("")
                .contains("Could not resolve host"),
            "original stderr must be preserved verbatim: {v:?}"
        );
        assert!(
            gate.requests().is_empty(),
            "network-pattern stderr must NOT trigger an unsandboxed-retry prompt: {:?}",
            gate.requests()
        );
    }

    #[tokio::test]
    async fn truncation_message_includes_byte_count() {
        let big = vec![b'a'; MAX_OUTPUT_BYTES + 4096];
        let total = big.len();
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: big,
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool::for_test()
            .execute(json!({ "command": "yes" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        let stdout = v["stdout"].as_str().unwrap();
        assert!(stdout.ends_with("] ..."), "stdout tail: {stdout:?}");
        assert!(
            stdout.contains(&format!("total {total}")),
            "missing total in: {stdout:?}"
        );
        assert!(
            stdout.contains("truncated 4096 bytes"),
            "missing elided count in: {stdout:?}"
        );
    }

    #[test]
    fn is_file_tool_redirect_catches_leading_argv0_only() {
        // FileToolRedirect is a tool-layer policy: only the literal
        // leading argv0 counts. Wrapped or env-prefixed variants
        // (`xargs cat`, `LANG=C cat`, `timeout 5 head`) deliberately
        // route through the sandbox path so the LLM still has the
        // escape hatch when it really needs `cat` in a pipeline.
        for cmd in [
            "cat foo.txt",
            "head -n 10 a.log",
            "/usr/bin/less x",
            "sed -i 's/a/b/' f",
            "awk '{print $1}' f",
            "gawk -F, '{print}' f",
            "mawk '/x/' f",
        ] {
            assert!(
                is_file_tool_redirect(cmd),
                "leading argv0 must trigger redirect for {cmd:?}"
            );
        }

        for cmd in [
            "echo hi",
            "grep foo bar.txt",
            "xargs cat foo",
            "LANG=C cat foo",
            "timeout 5 head log",
            "git log | sed 's/foo/bar/'",
            "",
        ] {
            assert!(
                !is_file_tool_redirect(cmd),
                "non-leading or unrelated argv0 must NOT trigger for {cmd:?}"
            );
        }
    }

    #[test]
    fn first_token_strips_path_but_not_wrappers_or_env() {
        assert_eq!(first_token("ls /etc"), Some("ls"));
        assert_eq!(first_token("/usr/bin/stat foo"), Some("stat"));
        // Wrappers and env-vars are NOT unwrapped — that's the point.
        assert_eq!(first_token("xargs ls"), Some("xargs"));
        assert_eq!(first_token("timeout 5 ls"), Some("timeout"));
        assert_eq!(first_token("LANG=C ls"), None);
        assert_eq!(first_token(""), None);
    }

    #[test]
    fn accessed_resources_only_prompts_for_delete_tokens() {
        // FileToolRedirect rejection bypasses approval — the sandbox
        // never gets reached.
        let resources = BashTool::for_test().accessed_resources(&json!({ "command": "cat foo" }));
        assert!(
            resources.is_empty(),
            "content-read is rejected before sandbox; no approval needed"
        );

        // Sandboxed-but-non-destructive: the sandbox is the gate, no
        // approval prompt fires up front. This includes the commands
        // that USED to qualify for the metadata fast lane.
        for cmd in [
            "echo hi",
            "git status",
            "cargo build",
            "ls /tmp; ls /var",
            "ls /etc",
            "pwd",
            "stat /tmp/x",
        ] {
            let resources = BashTool::for_test().accessed_resources(&json!({ "command": cmd }));
            assert!(
                resources.is_empty(),
                "{cmd:?} must skip pre-execution approval, got {resources:?}"
            );
        }

        // Sandboxed AND destructive: pre-execution approval fires.
        for cmd in [
            "rm /tmp/foo",
            "rm -rf /workspace/scratch",
            "/usr/bin/rm /tmp/x",
            "rmdir /tmp/empty",
            "unlink /tmp/foo",
            "shred -u secret",
            "find . -name '*.tmp' -delete",
            "find /tmp -type f -exec rm {} \\;",
            "ls /tmp; rm -rf /tmp/scratch",
            "git status && rm /tmp/foo",
            "echo hi | xargs rm",
            "git rm src/legacy.rs",
            // git destructive ops must also pop approval.
            "git clean -fd",
            "git reset --hard origin/main",
            "git branch -D feature",
            "git push --force origin main",
            "git stash drop",
            "git worktree remove /tmp/wt",
        ] {
            let resources = BashTool::for_test().accessed_resources(&json!({ "command": cmd }));
            assert_eq!(
                resources.len(),
                1,
                "{cmd:?} should declare ExecCommand for approval, got {resources:?}"
            );
        }
    }

    #[test]
    fn destructive_command_call_label_warns_user() {
        // Delete-bearing commands surface a warning label that the
        // approval UI renders alongside the JSON preview, so the user
        // notices the irreversible action before clicking through.
        for cmd in [
            "rm /tmp/foo",
            "rm -rf /workspace/scratch",
            "find . -delete",
            "git clean -fd",
            "git reset --hard origin/main",
            "xargs rm",
        ] {
            let label = BashTool::for_test().call_label(&json!({ "command": cmd }));
            let label = label.unwrap_or_else(|| panic!("expected warning label for {cmd:?}"));
            assert!(
                label.contains("Destructive") && label.contains("irreversible"),
                "warning must mention destructive + irreversible for {cmd:?}, got {label:?}"
            );
        }

        // Benign commands surface no label so the prompt stays clean
        // (and the approval gate doesn't fire at all for these — the
        // label only appears as part of the overall warning UX).
        for cmd in ["echo hi", "git status", "cargo build", "ls /tmp"] {
            assert_eq!(
                BashTool::for_test().call_label(&json!({ "command": cmd })),
                None,
                "benign command {cmd:?} must not surface a warning label"
            );
        }
    }

    #[tokio::test]
    async fn rejects_file_tool_redirect_commands_with_read_tool_hint() {
        for cmd in [
            "cat foo",
            "head -n 5 a",
            "tail b",
            "less c",
            "more d",
            "tac e",
            "sed -i 's/a/b/' f",
            "awk '{print}' f",
        ] {
            let err = BashTool::for_test()
                .execute(json!({ "command": cmd }), &ctx_with(None))
                .await
                .unwrap_err();
            let ToolError::InvalidParams(msg) = err else {
                panic!("expected InvalidParams for {cmd:?}, got: {err:?}");
            };
            assert!(
                msg.contains("Read") && msg.contains("Edit"),
                "rejection for {cmd:?} should mention both Read and Edit: {msg}"
            );
        }
    }

    #[test]
    fn contains_delete_command_handles_quoting_and_paths() {
        // Real delete invocations.
        for cmd in [
            "rm /foo",
            "rm -rf /foo",
            "/usr/bin/rm /foo",
            "rmdir /foo",
            "unlink /foo",
            "shred -u secret.txt",
            "srm sensitive",
            "wipe -r /foo",
            "find . -delete",
            "find . -name foo -delete",
            "ls; rm /foo",
            "git status && rmdir /foo",
            "echo go | xargs rm",
            "(rm /foo)",
            "$(rm /foo)",
            "git rm path/to/file",
        ] {
            assert!(
                contains_delete_command(cmd),
                "expected delete detection for {cmd:?}"
            );
        }

        // Quoted strings that contain "rm" as a substring must NOT
        // false-match — the literal rm token is inside an opaque
        // quoted region.
        for cmd in [
            "echo 'this is harmless rm text'",
            "echo \"saying rm here is fine\"",
            "grep 'rm' file.txt",
            "git log --grep='rm fix'",
        ] {
            assert!(
                !contains_delete_command(cmd),
                "quoted rm must not trigger detection in {cmd:?}"
            );
        }

        // Substring-only matches (rmail, format, removal, …) MUST NOT
        // trigger detection. The token equality check covers this.
        for cmd in [
            "rmail user",
            "format /dev/sdb",
            "echo removal in progress",
            "grep -rm 5 needle .",
        ] {
            assert!(
                !contains_delete_command(cmd),
                "substring of delete word must not trigger in {cmd:?}"
            );
        }
    }

    #[test]
    fn contains_delete_command_handles_wrappers_and_env_prefix() {
        // Wrappers descend into their wrapped command. Each line is an
        // explicit destructive invocation that must trigger detection.
        for cmd in [
            "xargs rm",
            "xargs -I {} rm /tmp/foo",
            "xargs -n 5 rm",
            "xargs -P 4 -n 1 rm",
            "xargs -0 rm",
            "echo go | xargs rm",
            "find /tmp -print0 | xargs -0 -n 5 rm",
            "nohup rm /foo",
            "sudo rm /foo",
            "sudo -u root rm /foo",
            "doas rm /foo",
            "nice rm /foo",
            "ionice rm /foo",
            "timeout 5 rm /foo",
            "env rm /foo",
            "env LANG=C rm /foo",
            "command rm /foo",
            "exec rm /foo",
            "xargs git clean -f",
            "sudo git reset --hard origin/main",
            "xargs git rm",
        ] {
            assert!(
                contains_delete_command(cmd),
                "wrapper-wrapped destructive must trigger in {cmd:?}"
            );
        }

        // `KEY=VAL` env-var prefixes don't shift the argv0 — the actual
        // argv0 still gets checked.
        for cmd in [
            "LANG=C rm /foo",
            "LC_ALL=C TZ=UTC rm -rf /tmp/scratch",
            "GIT_DIR=/tmp/g git clean -f",
        ] {
            assert!(
                contains_delete_command(cmd),
                "env-prefixed destructive must trigger in {cmd:?}"
            );
        }

        // Wrapper chains where the wrapped command is benign — must not
        // trigger even though our wrapper-descent is intentionally
        // aggressive about scanning every non-flag token.
        for cmd in [
            "nohup grep foo /tmp/file",
            "sudo cat /etc/passwd",
            "timeout 30 cargo build",
            "env LANG=C ls /tmp",
            "xargs ls",
        ] {
            assert!(
                !contains_delete_command(cmd),
                "wrapper around benign command must NOT trigger in {cmd:?}"
            );
        }
    }

    #[test]
    fn contains_delete_command_skips_argument_tokens() {
        // Non-wrapper commands take their tokens as ARGUMENTS, not as
        // potential argv0s. `grep rm`, `echo rm`, `printf rm` must NOT
        // trigger — the prior flat-token scan false-positived these.
        for cmd in [
            "grep rm /etc/passwd",
            "grep 'rm' file.txt",
            "echo rm",
            "echo \"will rm later\"",
            "printf 'rm %s\\n' x",
            "awk '/rm/' file.txt",
            "find /tmp -name rm",
            "sed 's/foo/rm/' file",
            // unrelated commands whose args mention delete-like words
            "ls /tmp/rm.bak",
            "stat -c '%n' rm.txt",
            // git pathspecs containing 'rm' as a path are not git rm
            "git log -- path/rm.rs",
            "git diff path/rm.rs",
        ] {
            assert!(
                !contains_delete_command(cmd),
                "non-wrapper argument token must NOT trigger detection in {cmd:?}"
            );
        }
    }

    #[test]
    fn contains_delete_command_unquotes_argv0() {
        // Codex flagged the prior shape: surrounding the argv0 with
        // quotes (or splicing them mid-word) used to slip past the
        // delete detector because the token text still carried the
        // quote chars. shell_words::split on each token now mirrors
        // what `sh -c` actually execs.
        for cmd in [
            "'rm' /tmp/foo",
            "\"rm\" /tmp/foo",
            "/bin/'rm' /tmp/foo",
            "/usr/bin/\"rm\" /tmp/foo",
            "r'm' /tmp/foo",
            "r\"m\" /tmp/foo",
            "\"r\"\"m\" /tmp/foo",
            "\\rm /tmp/foo",
            "\"git\" reset --hard",
            "'git' clean -f",
            "'git' rm src/legacy.rs",
            "\"git\" -C /tmp/repo clean -fd",
        ] {
            assert!(
                contains_delete_command(cmd),
                "quoted argv0 must still trigger detection in {cmd:?}"
            );
        }

        // Unmatched quotes / parse errors fail closed — the user gets
        // a prompt rather than a silent bypass.
        for cmd in ["'rm /tmp/foo", "\"git reset --hard"] {
            assert!(
                contains_delete_command(cmd),
                "shell parse error must fail closed in {cmd:?}"
            );
        }
    }

    #[test]
    fn contains_delete_command_catches_destructive_git() {
        // Real destructive git invocations across the supported subcommands
        // and global-option shapes.
        for cmd in [
            "git clean -f",
            "git clean -fd",
            "git clean -fdx",
            "git clean --force",
            "git -C /tmp/repo clean -f",
            "git -c color.ui=false clean -f",
            "git --git-dir=/tmp/g clean -fd",
            "git --git-dir /tmp/g --work-tree /tmp/w clean -f",
            "git -p clean -f",
            "git reset --hard",
            "git reset --hard HEAD",
            "git reset --hard origin/main",
            "git branch -d topic",
            "git branch -D feature",
            "git branch --delete topic",
            "git branch --delete --force topic",
            "git tag -d v1",
            "git tag --delete v1",
            "git push -f",
            "git push --force",
            "git push --force-with-lease",
            "git push --force-with-lease=origin/main",
            "git push --force-if-includes origin main",
            "git push --delete origin feature",
            "git push -d origin feature",
            "git stash drop",
            "git stash drop stash@{1}",
            "git stash clear",
            "git worktree remove /tmp/wt",
            "git update-ref -d refs/heads/x",
            "git update-ref --delete refs/heads/x",
            "git filter-branch --tree-filter 'true' HEAD",
            "git filter-repo --invert-paths --path secret",
            // Pipelines / chains: the destructive sub-command sits next
            // to a benign one and must still be flagged.
            "git status; git clean -f",
            "git fetch && git reset --hard origin/main",
        ] {
            assert!(
                contains_delete_command(cmd),
                "expected destructive-git detection for {cmd:?}"
            );
        }

        // Benign git invocations must NOT trigger.
        for cmd in [
            "git status",
            "git log --oneline",
            "git diff",
            "git diff --stat",
            "git branch",
            "git branch -a",
            "git branch -m new-name",
            "git branch -v",
            "git tag",
            "git tag -l",
            "git tag v1",
            "git tag --sort=-v:refname",
            "git push",
            "git push origin main",
            "git push --tags",
            "git fetch",
            "git pull",
            "git checkout main",
            "git checkout .",
            "git restore .",
            "git restore --staged .",
            "git stash",
            "git stash push",
            "git stash list",
            "git stash apply",
            "git stash pop",
            "git reset HEAD",
            "git reset --soft HEAD~",
            "git reset --mixed HEAD~",
            "git clean -n",
            "git clean -dn",
            "git clean -i",
            "git worktree list",
            "git worktree add /tmp/wt feature",
            "git update-ref refs/heads/x abc123",
            "git rebase -i HEAD~3",
            "git commit --amend",
        ] {
            assert!(
                !contains_delete_command(cmd),
                "benign git command must not trigger detection in {cmd:?}"
            );
        }
    }

    #[tokio::test]
    async fn benign_sandboxed_command_runs_without_pre_approval_prompt() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: b"clean\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        // The pre-execution gate is wired through the registry, not
        // BashTool::execute itself, so no prompt fires here regardless.
        // What this test pins is the *resource declaration*: empty for
        // benign commands so the registry has nothing to gate.
        let resources =
            BashTool::for_test().accessed_resources(&json!({ "command": "git status" }));
        assert!(resources.is_empty());

        let out = BashTool::for_test()
            .execute(json!({ "command": "git status" }), &ctx)
            .await
            .expect("benign command must run");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(
            gate.requests().is_empty(),
            "benign command must not pop any in-flight approval prompt: {:?}",
            gate.requests()
        );
    }

    #[tokio::test]
    async fn unsandboxed_runner_exports_pwd_from_effective_cwd() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let out = run_unsandboxed("env", &[], Some(workspace.path()), Duration::from_secs(5))
            .await
            .expect("env must run");
        let stdout = String::from_utf8(out.stdout).expect("env stdout");
        let pwd = stdout
            .lines()
            .find_map(|line| line.strip_prefix("PWD="))
            .expect("PWD in env");
        let actual = Path::new(pwd).canonicalize().expect("PWD path");
        let expected = workspace.path().canonicalize().expect("workspace path");

        assert_eq!(actual, expected);
    }
}
