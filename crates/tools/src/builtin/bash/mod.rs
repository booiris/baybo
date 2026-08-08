//! `Bash` — execute a shell command via `sh -c` under the configured safety
//! policy.
//!
//! In `permission = auto` or `manual`, shell commands run through bwrap
//! (Linux) / sandbox-exec (macOS) / docker first when the runtime has a usable
//! inner sandbox. If the gateway detects an outer sandbox/container, Bash
//! silently skips the inner OS sandbox; if no sandbox backend is available on a
//! non-container host, Bash sends a notice and runs without the inner OS
//! sandbox under the same approval policy. `free` runs directly without the
//! OS sandbox. Invocations of the local `baybo` CLI (any sub-command whose
//! argv0 is [`baybo_workspace::paths::BIN_NAME`]) are the exception: the
//! sandbox masks the Baybo state dir (`~/.baybo`/`$BAYBO_HOME`), so a sandboxed
//! `baybo …` call can't see the parent gateway's config or session store.
//! Running it sandboxed is broken by construction, so the agent's own CLI gets
//! the unsandboxed `sh -c` path directly.
//!
//! The OS sandbox runs in **permissive filesystem** mode capped at
//! `workspace_root + $HOME`: FHS roots
//! (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`,
//! `/run/systemd/resolve`) stay RO so installed binaries and
//! resolv.conf still work; credential vaults (`~/.ssh`, `~/.aws`,
//! `~/.gnupg`, `~/.gpg`, `~/.config/gh`, `~/.config/gcloud`,
//! `~/.docker`, `~/.kube`, and the Baybo state dir under
//! `~/.baybo`/`$BAYBO_HOME`) are masked with per-call empty `tmpfs`;
//! `/dev` is a fresh minimal devtmpfs (no host raw devices); network
//! is enabled. Anything outside `workspace_root + $HOME + FHS-RO` is
//! invisible inside the sandbox.
//!
//! File-content viewers (`cat`, `head`, `tail`, `sed`, `awk`, …) are
//! rejected at the tool layer to force the Read/Edit tools.
//!
//! `permission` controls approvals and isolation: `auto` risk-judges
//! destructive commands and sandbox-failure escapes, `manual` declares every
//! Bash command to the executor's approval gate and asks before sandbox escape,
//! and `free` runs directly with no Bash approval. Environment variables and
//! `cd` changes do NOT persist across invocations.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use baybo_trace::ToolEventPayload;
use baybo_workspace::{WorkspacePaths, absolutise};
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use std::sync::Arc;

use baybo_model::new_background_handle;

use crate::{
    ApprovalDecision, BackgroundJobSink, DetachedCommand, NoticeLevel, ResourceAccess,
    RunningChild, SpawnOpts, Tool, ToolContext, ToolError, ToolOutput,
};

mod judge;
mod parse;

use judge::{PostFail, PreExec, judge_post_fail, judge_pre_exec};
use parse::{
    DELETE_SCAN_EVENT_ACTION, contains_delete_command, first_token, is_env_assignment,
    parse_program, split_into_subcommands, truncate_for_event,
};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_KIB: usize = MAX_OUTPUT_BYTES / 1024;
/// Cap for the one-line command digest logged on sandbox-bypass events.
const COMMAND_HEAD_MAX_CHARS: usize = 160;

/// One-line, log-safe digest of a shell command for a structured log
/// field: the first line with control chars flattened via `escape_debug`
/// (so a multi-line script can never spill into bare, un-timestamped log
/// continuation lines), truncated on a char boundary. The full command
/// still reaches the user notice and the persisted tool-call trace span.
fn command_head(command: &str) -> String {
    command
        .lines()
        .next()
        .unwrap_or_default()
        .escape_debug()
        .take(COMMAND_HEAD_MAX_CHARS)
        .collect()
}

/// Shared Bash tool description. Four sections vary by permission —
/// `{{isolation}}` (the FS/network surface), `{{approval}}` (the gate),
/// `{{work_dir_scope}}` (writability + the `work/tmp` SCRATCH advertisement,
/// which must not reach the bench profile: there is no janitor in a bench
/// container, so a swept-scratch promise there would be false), and
/// `{{python_runtime}}` (uv-shimmed vs native) — substituted by
/// [`render_description`] along with `{{max_output_kib}}`, `{{work_dir}}`,
/// `{{work_tmp_dir}}`, `{{work_tmp_ttl_days}}`, and `{{platform}}`. Each
/// varying section describes ONLY its own concern so a permission swap
/// re-skins exactly what changed and nothing is said twice. Work-dir/platform
/// live here, not the system prompt, so they sit next to the tool that
/// consumes them.
const DESCRIPTION_TEMPLATE: &str = r#"Execute a shell command in a fresh `sh -c` process. Environment changes and `cd` do not persist across invocations. Each of stdout and stderr is truncated at {{max_output_kib}} KiB.

Reserve Bash for system commands, git, build/test, and anything that genuinely needs a shell. Reading, writing, and searching files have dedicated tools — `Read`, `Write`, `Edit`, `Glob`, `Grep` — and a leading `cat`/`head`/`tail`/`less`/`sed`/`awk` against a file is rejected here with the redirect spelled out. Downloading a file to disk IS Bash's job (`curl`/`wget`): WebFetch only returns rendered text into the conversation and never writes to disk.

{{isolation}}

{{approval}}

DEFAULT CWD: If `cwd` is omitted, Baybo runs the command from {{work_dir}} and exports `PWD` with the same value.

PATHS: Every directory or file argument, and `cwd` itself, MUST be absolute; relative values are rejected. Quote paths containing spaces.

{{work_dir_scope}}

BEFORE BROAD SCANS: Do not run `find`, `du`, recursive `ls`, or similar walks against unknown directories without first checking their size with a bounded probe (e.g. `ls -1 <dir> | wc -l`, or a shallow `find -maxdepth 2`). Large trees can hang the process.

{{python_runtime}}

ENVIRONMENT:
- Working directory: {{work_dir}}
- Platform: {{platform}}"#;

#[cfg(not(feature = "bench-bash"))]
const SANDBOXED_ISOLATION: &str = r#"SANDBOX: The shell has read+write access to the workspace and `$HOME`, with the FHS roots (`/usr`, `/bin`, `/etc`, …) readable; nothing outside that union exists. The usual credential directories under `$HOME` (ssh, aws, gnupg, gh, gcloud, docker, kube, and Baybo's own state dir) are masked and read as empty, and `/dev` is minimal. Network is enabled."#;

#[cfg(not(feature = "bench-bash"))]
const SANDBOXED_WORK_DIR_SCOPE: &str = r#"WORK-DIR SCOPE: Inside the workspace, Bash may only name {{work_dir}} (read+write) and `skills/` (read+execute, never write). Every other path under the workspace root is rejected up front, `cwd` included — reach those through `Read`/`Edit`/`Write` instead. Paths outside the workspace are unaffected by this rule.

SCRATCH: Put disposable/intermediate files (probe scripts, one-off downloads, temp build output) under {{work_tmp_dir}} — it is swept automatically after {{work_tmp_ttl_days}} days. Deliverables the user should keep belong elsewhere under {{work_dir}}."#;

#[cfg(not(feature = "bench-bash"))]
const SANDBOXED_PYTHON: &str = r#"PYTHON: `python`, `python3`, and `pip` are shimmed to `uv run python` / `uv pip` inside this shell. For one-file scripts with third-party deps, declare them via PEP 723 inline metadata (`# /// script` block) so `uv run --script my.py` resolves them per-call. The shims are shell functions scoped to the outer `sh -c` — `bash -c '…'` subshells, `/usr/bin/python`, and Python's own `subprocess` calls bypass them."#;

/// `{{isolation}}` for `permission = free`: only the OS sandbox is
/// dropped (the work-dir jail and uv shim still apply — described by their own
/// sections).
#[cfg(not(feature = "bench-bash"))]
const FREE_ISOLATION: &str = r#"SANDBOX: The OS sandbox is OFF — commands run directly via `sh -c` on the host: no bwrap, no credential-vault masking, no resource caps, and the host filesystem is reachable. Network is enabled."#;

/// `{{approval}}` for `permission = manual`: human approval for every
/// Bash command.
#[cfg(not(feature = "bench-bash"))]
const MANUAL_APPROVAL: &str = r#"APPROVAL: Every Bash command needs human approval before it runs, and approved commands run sandboxed where a runner exists. A sandboxed run that fails is asked about again before any unsandboxed retry. Commands Bash rejects outright are refused without ever reaching the prompt."#;

/// `{{approval}}` for `permission = auto`.
#[cfg(not(feature = "bench-bash"))]
const AUTO_APPROVAL: &str = r#"APPROVAL: Commands run sandboxed without prompting. A destructive one (file deletion, a history-rewriting `git` op) is risk-judged first, and you are asked only when the judge flags it. Sandbox failures and escapes are handled for you; a run that ended up unsandboxed says so in a `sandbox_escalation` field."#;

/// `{{approval}}` for `permission = free`.
#[cfg(not(feature = "bench-bash"))]
const FREE_APPROVAL: &str = r#"APPROVAL: Bash is free: commands run directly without Bash pre-execution approval, destructive-command judging, or sandbox-failure escalation."#;

// ── Prompt sections for the `bench-bash` profile: raw container exec ──────────
// No OS sandbox, no work-dir jail, no uv shim, inherited cwd, no gate. Compiled
// only with the feature; the normal-build sections above are absent then.

#[cfg(feature = "bench-bash")]
const BENCH_ISOLATION: &str = r#"EXECUTION: The shell runs directly on the host filesystem with full read+write access — no OS sandbox and no masked paths (this environment is already disposable/isolated). Network is enabled."#;

#[cfg(feature = "bench-bash")]
const BENCH_WORK_DIR_SCOPE: &str = r#"WORK-DIR SCOPE: {{work_dir}} is your working directory — create and modify files there, including with the `Write`/`Edit` tools (give them absolute paths under it). There is no work-dir jail; just keep output under {{work_dir}}."#;

#[cfg(feature = "bench-bash")]
const BENCH_PYTHON: &str = r#"PYTHON: `python`, `python3`, and `pip` are the host's own interpreters — run them directly. There is no uv shim, so call them as-is (no `uv run` or PEP 723 `--script` needed)."#;

#[cfg(feature = "bench-bash")]
const BENCH_APPROVAL: &str = r#"APPROVAL: Commands run directly with no approval gate."#;

/// Compile-time switch for the bench profile (the `bench-bash` feature). When
/// true, the Bash tool ignores the configured permission for execution shape and always
/// runs raw — no OS sandbox, no uv shim, no work-dir jail, inherited cwd, the
/// bench prompt, and no approval gate.
const BENCH: bool = cfg!(feature = "bench-bash");

fn permission_skips_os_sandbox(permission: BashPermissionMode) -> bool {
    BENCH || permission == BashPermissionMode::Free
}

/// How Bash handles approval and sandbox escape.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BashPermissionMode {
    #[default]
    Auto = 0,
    Manual = 1,
    Free = 2,
}

impl BashPermissionMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => BashPermissionMode::Manual,
            2 => BashPermissionMode::Free,
            _ => BashPermissionMode::Auto,
        }
    }

    fn encode(self) -> u8 {
        self as u8
    }
}

/// Shared, hot-swappable Bash permission mode. A config reload calls [`Self::set`] and
/// every `BashTool` holding the `Arc` sees it on its next call — both the
/// execution path and the live-rendered tool description. Lock-free: the mode
/// is a single byte read on the per-command hot path.
pub struct LivePermissionMode(std::sync::atomic::AtomicU8);

impl LivePermissionMode {
    pub fn new(permission: BashPermissionMode) -> Self {
        Self(std::sync::atomic::AtomicU8::new(permission.encode()))
    }

    pub fn get(&self) -> BashPermissionMode {
        BashPermissionMode::from_u8(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn set(&self, permission: BashPermissionMode) {
        self.0
            .store(permission.encode(), std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a command's child is given, and what must never come back out.
#[derive(Debug, Default, Clone)]
struct ChildEnv {
    /// Every variable the child process gets.
    vars: Vec<(String, String)>,
    /// Values scrubbed from stdout and stderr before anything sees them.
    /// The resolved user secrets and nothing else — this is a filter, not
    /// a record of what was injected.
    secret_values: Vec<String>,
}

fn git_identity(
    agent: &baybo_model::AgentProfileId,
    handle: &baybo_model::AgentHandle,
) -> Vec<(String, String)> {
    let name = handle.as_str().to_owned();
    let email = format!("{}@baybo.local", agent.as_str());
    vec![
        ("GIT_AUTHOR_NAME".to_owned(), name.clone()),
        ("GIT_AUTHOR_EMAIL".to_owned(), email.clone()),
        ("GIT_COMMITTER_NAME".to_owned(), name),
        ("GIT_COMMITTER_EMAIL".to_owned(), email),
    ]
}

pub struct BashTool {
    /// Tool descriptions pre-rendered per [`BashPermissionMode`], indexed by
    /// the permission's encoded byte. [`Tool::description`] returns the one for the
    /// current hot-swappable permission, so a `permission` reload re-skins the prompt
    /// the LLM sees without rebuilding the tool. The `bench-bash` build has a
    /// single fixed prompt instead (the `bench_description` field).
    #[cfg(not(feature = "bench-bash"))]
    descriptions: [String; 3],
    /// The one fixed bench-profile description (inherited cwd, no jail, native
    /// python). Only exists — and is only rendered — under the `bench-bash`
    /// feature, which overrides the per-permission descriptions.
    #[cfg(feature = "bench-bash")]
    bench_description: String,
    /// Absolute workspace root (`<workspace>`). Used together with
    /// [`Self::work_dir`] to reject path arguments that would touch
    /// non-`work/` subtrees (`personas/`, `config/`, `state/`, …).
    workspace_root: PathBuf,
    /// Absolute work directory (`<workspace>/work`). Sole writable area
    /// for Bash invocations.
    work_dir: PathBuf,
    /// Pre-rendered `export UV_*=…; ` chain prepended to every command so
    /// any `uv` invocation caches inside the workspace rather than
    /// `~/.cache/uv` / `~/.local/share/uv`. Non-uv processes inherit and
    /// ignore the variables — same loose-coupling rationale as
    /// [`inject_baybo_env`].
    uv_env_prefix: String,
    process_manager: Arc<baybo_process::ProcessManager>,
    /// Shared, hot-swappable permission mode. Read on every call (and by
    /// [`Tool::description`]); a config reload swaps it via [`LivePermissionMode::set`].
    permission: Arc<LivePermissionMode>,
}

impl BashTool {
    pub fn new(
        workspace_paths: WorkspacePaths,
        process_manager: Arc<baybo_process::ProcessManager>,
    ) -> Self {
        // Re-anchor at the absolutised root so the env-var values
        // rendered by `build_uv_env_exports` are absolute regardless
        // of whether `config.workspace.path` came in absolute — the
        // subshell inherits these as-is and tools running with a
        // different cwd must still resolve them.
        let paths = WorkspacePaths::new(absolutise(workspace_paths.root()));
        let workspace_root = paths.root().to_path_buf();
        let work_dir = paths.work_dir();
        Self {
            #[cfg(not(feature = "bench-bash"))]
            descriptions: build_descriptions(&work_dir, std::env::consts::OS),
            #[cfg(feature = "bench-bash")]
            bench_description: render_bench_description(std::env::consts::OS),
            workspace_root,
            work_dir,
            uv_env_prefix: build_uv_env_exports(&paths, resolve_uv_bin_dir().as_deref()),
            process_manager,
            permission: Arc::new(LivePermissionMode::new(BashPermissionMode::default())),
        }
    }

    /// The calling agent's skill directory — the one tree a command may name
    /// paths inside besides `work/`. It is bound read-only into the sandbox,
    /// and running an installed skill's bundled script in place is the point.
    /// Per call rather than per process because the agent decides it, and
    /// another agent's tree is neither bound nor exempt.
    fn skill_root(&self, ctx: &ToolContext) -> PathBuf {
        absolutise(&ctx.agent_id.skills_dir(&ctx.workspace_paths))
    }

    /// Pin a fixed permission mode via a fresh (non-shared) handle. For callers
    /// that don't participate in hot-reload — mainly tests.
    #[cfg(test)]
    pub fn with_permission(mut self, permission: BashPermissionMode) -> Self {
        self.permission = Arc::new(LivePermissionMode::new(permission));
        self
    }

    /// Share a hot-swappable permission handle (the production path): a config reload
    /// calls [`LivePermissionMode::set`] on it and this tool's next call — and its
    /// next [`Tool::description`] — observe the new mode.
    pub fn with_permission_handle(mut self, permission: Arc<LivePermissionMode>) -> Self {
        self.permission = permission;
        self
    }

    fn permission(&self) -> BashPermissionMode {
        self.permission.get()
    }

    /// Live read of whether the OS sandbox is skipped. `execute` does NOT call
    /// this — it snapshots permission once so one command keeps a consistent
    /// execution shape. Kept as a point-in-time accessor for tests.
    #[cfg(test)]
    fn skip_os_sandbox(&self) -> bool {
        permission_skips_os_sandbox(self.permission())
    }

    /// Prefix `command` with the workspace-scoped UV exports and the
    /// Baybo-CLI env injection. Two callers (the sandboxed `execute` path
    /// and the unsandboxed retry below) compose the same `sh -c` body —
    /// keep the ordering in one place so a future reshuffle doesn't
    /// drift between them.
    fn wrap_command(&self, command: &str) -> String {
        let injected = inject_baybo_env(command);
        // Only the bench profile skips the uv shims/exports: that container ships
        // its own python/pip and has no `uv`, so the `python() { uv run python …; }`
        // shim would turn every `python`/`pip` into `uv run …` → `uv: not found`.
        // `permission = free` keeps uv (it only drops the OS sandbox).
        if BENCH {
            return injected;
        }
        let mut out = String::with_capacity(self.uv_env_prefix.len() + injected.len());
        out.push_str(&self.uv_env_prefix);
        out.push_str(&injected);
        out
    }
}

/// Render the three [`BashPermissionMode`] descriptions, indexed by the
/// encoded permission byte. `free` drops only the OS sandbox; the work-dir jail
/// and uv shim stay. Compiled out under `bench-bash` (which renders one fixed
/// prompt — see `render_bench_description`).
#[cfg(not(feature = "bench-bash"))]
fn build_descriptions(work_dir: &Path, platform: &str) -> [String; 3] {
    let mut out: [String; 3] = Default::default();
    for (permission, isolation, approval) in [
        (BashPermissionMode::Auto, SANDBOXED_ISOLATION, AUTO_APPROVAL),
        (
            BashPermissionMode::Manual,
            SANDBOXED_ISOLATION,
            MANUAL_APPROVAL,
        ),
        (BashPermissionMode::Free, FREE_ISOLATION, FREE_APPROVAL),
    ] {
        out[permission.encode() as usize] = render_description(
            isolation,
            SANDBOXED_WORK_DIR_SCOPE,
            SANDBOXED_PYTHON,
            approval,
            work_dir,
            platform,
        );
    }
    out
}

/// The `bench-bash` profile description: inherited cwd as the work dir, no jail,
/// native python, no approval gate. Compiled (and rendered) only under the
/// feature — it's the single prompt the bench build advertises.
#[cfg(feature = "bench-bash")]
fn render_bench_description(platform: &str) -> String {
    let cwd =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("(working directory)"));
    render_description(
        BENCH_ISOLATION,
        BENCH_WORK_DIR_SCOPE,
        BENCH_PYTHON,
        BENCH_APPROVAL,
        &cwd,
        platform,
    )
}

/// Fill the shared [`DESCRIPTION_TEMPLATE`]: the permission-specific sections first,
/// then the value placeholders (so a `{{work_dir}}` inside an inserted section
/// is resolved too).
fn render_description(
    isolation: &str,
    work_dir_scope: &str,
    python_runtime: &str,
    approval: &str,
    work_dir: &Path,
    platform: &str,
) -> String {
    DESCRIPTION_TEMPLATE
        .replace("{{isolation}}", isolation)
        .replace("{{work_dir_scope}}", work_dir_scope)
        .replace("{{python_runtime}}", python_runtime)
        .replace("{{approval}}", approval)
        .replace("{{max_output_kib}}", &MAX_OUTPUT_KIB.to_string())
        .replace(
            "{{work_tmp_dir}}",
            &work_dir
                .join(baybo_workspace::paths::WORK_TMP_SUBDIR)
                .display()
                .to_string(),
        )
        .replace(
            "{{work_tmp_ttl_days}}",
            &baybo_workspace::paths::WORK_TMP_TTL_DAYS.to_string(),
        )
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
/// without a separator. `uv_dir`, when present, is uv's install directory
/// resolved off the gateway's `$PATH`; it is folded onto the front of the
/// in-sandbox `$PATH` so the python/pip shims can find `uv` even though the
/// bwrap sandbox hands the command a `--clearenv`'d, hardcoded
/// `PATH=/usr/bin:/bin:/usr/sbin:/sbin` (`baybo-sandbox`) that omits the
/// `~/.local/bin` location uv's own installer uses. `None` (uv not on PATH)
/// leaves PATH untouched — the shims then fail only if the agent actually
/// invokes python.
fn build_uv_env_exports(paths: &WorkspacePaths, uv_dir: Option<&Path>) -> String {
    let mut out = String::new();
    if let Some(dir) = uv_dir {
        out.push_str("export PATH=");
        out.push_str(&sh_quote(&dir.to_string_lossy()));
        out.push_str(r#":"$PATH"; "#);
    }
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

/// Bare name of the uv binary, looked up on the gateway's `$PATH`.
const UV_BIN_NAME: &str = "uv";

/// Best-effort resolve of the directory holding the `uv` executable on the
/// gateway's `$PATH`, for [`build_uv_env_exports`] to splice onto the
/// sandbox PATH. Resolved once at [`BashTool::new`] from the gateway
/// process env, which carries the user's full PATH (uv's prewarm
/// [`spawn_uv_python_prewarm`] relies on the same). `None` when uv isn't
/// installed.
fn resolve_uv_bin_dir() -> Option<PathBuf> {
    uv_bin_dir_in(&std::env::var_os("PATH")?)
}

/// PATH-walk split out of [`resolve_uv_bin_dir`] so it is unit-testable
/// without mutating the process-global `PATH`. Empty entries (the `::`
/// implicit-cwd form) are skipped so cwd never lands on the sandbox PATH.
fn uv_bin_dir_in(path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find(|dir| is_executable_file(&dir.join(UV_BIN_NAME)))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    matches!(
        std::fs::metadata(path),
        Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0
    )
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
pub fn spawn_uv_python_prewarm(
    paths: &WorkspacePaths,
    process_manager: Arc<baybo_process::ProcessManager>,
) {
    let env: Vec<(&'static str, PathBuf)> = UV_ENV_VARS
        .iter()
        .map(|(name, get)| (*name, get(paths)))
        .collect();
    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new(UV_BIN_NAME);
        cmd.args(["python", "install", UV_PREWARM_PYTHON]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::piped());
        let outcome = match process_manager.spawn(&mut cmd, "uv-python-prewarm") {
            Ok(child) => child.wait_with_output().await,
            Err(error) => Err(error),
        };
        match outcome {
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
        let mut tool = Self::new(
            WorkspacePaths::new("/tmp"),
            baybo_process::ProcessManager::transient(),
        );
        tool.permission = Arc::new(LivePermissionMode::new(BashPermissionMode::Manual));
        tool
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    secret_env: Vec<String>,
    /// `"background"` (default) or `"kill"`: what to do when the command
    /// exceeds its timeout. `background` detaches it (you get a handle + a
    /// completion notification); `kill` keeps the old kill-on-timeout
    /// behaviour. Ignored when the turn is not
    /// [`ToolContext::background_eligible`](crate::ToolContext::background_eligible).
    #[serde(default)]
    on_timeout: Option<String>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> String {
        // Exactly one of the two fields exists per build (see their `#[cfg]`s).
        #[cfg(feature = "bench-bash")]
        let out = self.bench_description.clone();
        #[cfg(not(feature = "bench-bash"))]
        let out = self.descriptions[self.permission().encode() as usize].clone();
        out
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string", "description": "The shell command to run" },
                "timeout_ms": { "type": "integer", "minimum": 1, "description": "Per-command timeout in ms (falls back to the tool context timeout)" },
                "cwd":        { "type": "string", "description": "Working directory for the command" },
                "secret_env": { "type": "array", "items": { "type": "string" }, "description": "Names of stored user secrets to inject as environment variables for THIS command only. Values are pulled from the vault, never shown to you, and scrubbed from the output. Discover names with SecretList / SecretCheck." },
                "on_timeout": { "type": "string", "enum": ["background", "kill"], "description": "What to do if the command exceeds its timeout. 'background' (default) detaches it — you get a handle now and a notification when it finishes, with full output streamed to a file you can Read — so a long build/test never blocks you or loses its work. 'kill' keeps the old behaviour (terminate + return a timeout error). Ignored during a scheduled job's own run and in nested subagents, which always kill on timeout." }
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
        // Manual permission declares every executable Bash command to the
        // executor's human approval gate. Auto owns the destructive-command
        // gate inside `execute` via the LLM judge, and free disables Baybo's
        // pre-execution approval entirely. FileToolRedirect commands are
        // rejected before any spawn, so asking would be noise.
        if BENCH || self.permission() != BashPermissionMode::Manual {
            return Vec::new();
        }
        params
            .get("command")
            .and_then(|v| v.as_str())
            .filter(|s| !is_file_tool_redirect(s))
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
            // `free` keeps the work-dir jail (only the OS sandbox is dropped);
            // only the bench profile lifts it.
            if !BENCH {
                require_within_work_dir(dir, &self.workspace_root, &self.work_dir, "cwd")?;
            }
        }

        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.timeout);

        let command = p.command;
        if !BENCH {
            require_command_paths_within_work_dir(
                &command,
                &self.workspace_root,
                &self.work_dir,
                &self.skill_root(ctx),
            )?;
        }
        // The bench profile has no work-dir jail, so a command with no explicit
        // `cwd` runs from the process's inherited working directory — the
        // benchmark container's WORKDIR, where the task files live — rather than
        // the workspace work dir.
        let inherited_cwd = if BENCH && p.cwd.is_none() {
            Some(
                std::env::current_dir()
                    .map_err(|e| ToolError::Execution(format!("resolve current dir: {e}")))?,
            )
        } else {
            None
        };
        let cwd_ref: Option<&Path> = p
            .cwd
            .as_deref()
            .or(inherited_cwd.as_deref())
            .or(ctx.checkout_root.as_deref())
            .or(Some(ctx.workspace_root.as_path()));

        if is_file_tool_redirect(&command) {
            let argv0 = first_token(&command).unwrap_or("?");
            return Err(ToolError::InvalidParams(format!(
                "Refusing to run `{argv0}` against a file via Bash. Use the right tool \
                 for the turn:\n\
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

        // Record when the destructive-command detector's shell parser can't
        // parse this command — it then falls back to the fail-closed keyword
        // pre-filter, so surfacing the parse gap in the trace explains an
        // otherwise-mysterious approval prompt and flags parser gaps to fix.
        if parse_program(&command).is_none() {
            ctx.events.emit(
                DELETE_SCAN_EVENT_ACTION,
                ToolEventPayload::ParseFailure {
                    command: truncate_for_event(&command),
                },
            );
        }

        let permission = self.permission();
        let execution_route = bash_execution_route(&command, permission);
        if matches!(
            execution_route,
            BashExecutionRoute::RejectNonCanonicalBayboCliPath
        ) {
            // The agent is clearly trying to invoke baybo (basename
            // match) but used a bare/relative/wrong-absolute argv0.
            // Sandboxing would just fail opaquely on the masked
            // state dir; surface a precise instruction with the
            // correct absolute path so the agent can self-correct.
            let bin_display = BAYBO_BIN
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unknown>".into());
            return Err(ToolError::InvalidParams(format!(
                "Baybo CLI invocations must use the absolute path of the gateway binary. \
                 Replace the argv0 with `{bin_display}` (e.g. \
                 `{bin_display} cost` instead of `baybo cost`). \
                 Bare-name and relative-path invocations are rejected so the \
                 unsandboxed shell never resolves `baybo` through `$PATH`."
            )));
        }
        let sandbox_bypassed = execution_route.is_sandboxed() && ctx.sandbox.is_none();
        let effective_sandboxed = execution_route.is_sandboxed() && !sandbox_bypassed;
        let effective_escape_policy = if sandbox_bypassed {
            SandboxEscapePolicy::None
        } else {
            execution_route.escape_policy()
        };

        // Resolve any requested user secrets to plaintext for env injection.
        // Fail closed if requested but the secret store isn't wired. The names
        // (never the values) are recorded for audit; the values are scrubbed
        // back out of the output below. See docs/secret-management.md.
        let mut extra_env = ChildEnv::default();
        if !p.secret_env.is_empty() {
            let handle = ctx.secrets.as_deref().ok_or_else(|| {
                ToolError::Execution(
                    "secret_env was requested but no secret store is available in this context"
                        .into(),
                )
            })?;
            tracing::info!(
                target: "baybo::tools::bash",
                secrets = ?p.secret_env,
                "bash: injecting user secrets as environment variables"
            );
            extra_env.vars = handle.resolve_env(&p.secret_env).await?;
            extra_env.secret_values = extra_env.vars.iter().map(|(_, v)| v.clone()).collect();
        }
        if ctx.checkout_root.is_some()
            && let Some(handle) = ctx.agent_handle.as_ref()
        {
            extra_env.vars.extend(git_identity(&ctx.agent_id, handle));
        }

        // Auto permission, destructive-token command: the LLM judge decides before
        // running whether this needs human approval (replacing the blunt
        // token→always-prompt gate that `accessed_resources` defers in auto).
        let pre_exec_judge = execution_route.pre_exec_judge();
        if pre_exec_judge && contains_delete_command(&command) {
            self.pre_exec_gate(&command, cwd_ref, ctx, effective_sandboxed)
                .await?;
        }
        if sandbox_bypassed {
            self.notify_sandbox_bypass(ctx, &command);
        }

        let args = vec!["-c".into(), self.wrap_command(&command)];
        let detached_route = if sandbox_bypassed {
            Some(DetachedExecutionRoute::Unsandboxed)
        } else {
            execution_route.detached_route()
        };

        // Convertible path: when the turn may create background work the
        // default `on_timeout` detaches a command that overruns its budget
        // instead of killing it. `run_detached` returns `Some` on success (completed
        // in-window, or backgrounded) and `None` if it couldn't detach
        // (sandbox backend without detached support), in which case we fall
        // through to the blocking kill-on-timeout path below.
        // `secret_env` does not block detach. Background output files and
        // completion tails are raw; if secrets were injected, the background
        // arm below records that risk before handing the child to the sink.
        let convert_on_timeout = !p
            .on_timeout
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| s.eq_ignore_ascii_case("kill"));
        if convert_on_timeout
            && ctx.background_eligible
            && let Some(detached_route) = detached_route
            && let Some(sink) = ctx.background_jobs.clone()
            && let Some(output) = run_detached(
                self,
                detached_route,
                &command,
                &args,
                cwd_ref,
                &extra_env,
                timeout,
                ctx,
                &sink,
                effective_escape_policy,
            )
            .await?
        {
            return Ok(output);
        }

        let out = if execution_route.is_unsandboxed() || sandbox_bypassed {
            // The self-CLI, bench profile, and `permission = free` run
            // directly via `sh -c` — there is no OS sandbox to wrap with (the
            // bench container is already disposable; `free` opts out on the
            // host; sandbox bypass handles outer-container or missing-backend
            // downgrades after surfacing a notice).
            tokio::select! {
                _ = ctx.cancellation_token.cancelled() => {
                    return Err(ToolError::Execution("cancelled".into()));
                }
                res = run_unsandboxed(&self.process_manager, "sh", &args, cwd_ref, &extra_env.vars, timeout) => res?,
            }
        } else if execution_route.is_sandboxed() {
            if let Some(sandbox) = ctx.sandbox.as_ref() {
                let attempt = tokio::select! {
                    _ = ctx.cancellation_token.cancelled() => {
                        return Err(ToolError::Execution("cancelled".into()));
                    }
                    res = sandbox.spawn_command(
                        Path::new("sh"),
                        &args,
                        SpawnOpts {
                            cwd: cwd_ref.map(Path::to_path_buf),
                            stdin: None,
                            extra_env: extra_env.vars.clone(),
                            timeout,
                        },
                    ) => res,
                };
                match attempt {
                    Ok(out) => out,
                    Err(sandbox_err) => {
                        self.handle_sandbox_start_failure(
                            &command,
                            cwd_ref,
                            &extra_env,
                            timeout,
                            ctx,
                            sandbox_err,
                            effective_escape_policy,
                        )
                        .await?
                    }
                }
            } else {
                return Err(ToolError::Execution(
                    "internal error: sandbox route selected without a sandbox runner".into(),
                ));
            }
        } else {
            unreachable!("baybo CLI routes are handled before command dispatch")
        };

        if out.timed_out {
            return Err(ToolError::Timeout(format!("Bash exceeded {timeout:?}")));
        }

        // Auto permission: a failed sandboxed command may be re-run unsandboxed
        // (when the judge deems it safe + sandbox-related) or escalated to the
        // user.
        let (out, escalation) = self
            .escalate_if_failed(
                &command,
                cwd_ref,
                out,
                &extra_env,
                timeout,
                ctx,
                effective_escape_policy,
            )
            .await?;

        format_command_result(
            &command,
            out.exit_code,
            &out.stdout,
            &out.stderr,
            &extra_env,
            ctx,
            escalation.as_deref(),
        )
        .await
    }
}

/// Format a finished command into the tool result the LLM sees: truncated
/// stdout/stderr (secret values redacted), the exit code, and an optional
/// hint for well-known non-zero exits. Shared by the blocking path and the
/// detached path's foreground-completion case.
async fn format_command_result(
    command: &str,
    exit_code: i32,
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
    extra_env: &ChildEnv,
    ctx: &ToolContext,
    escalation_note: Option<&str>,
) -> crate::Result<ToolOutput> {
    let mut stdout = truncate_utf8(stdout_bytes, MAX_OUTPUT_BYTES);
    let mut stderr = truncate_utf8(stderr_bytes, MAX_OUTPUT_BYTES);
    // Scrub injected secret values out of the output before it reaches the
    // agent / LLM / trace — the leak detector only catches known formats,
    // so arbitrary user tokens are redacted here by exact match.
    if !extra_env.secret_values.is_empty()
        && let Some(handle) = ctx.secrets.as_deref()
    {
        stdout = handle.redact(&stdout, &extra_env.secret_values).await?;
        stderr = handle.redact(&stderr, &extra_env.secret_values).await?;
    }
    let mut result = json!({
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    });
    if let Some(hint) = interpret_exit(command, exit_code) {
        result["return_code_interpretation"] = Value::String(hint.into());
    }
    // Auto permission: tell the LLM this result came from an unsandboxed re-run
    // so it reasons about the elevated privilege rather than assuming the
    // sandbox.
    if let Some(note) = escalation_note {
        result["sandbox_escalation"] = Value::String(note.to_string());
    }
    Ok(ToolOutput::Json(result))
}

/// Per-stream cap for a detached command's output file. Mirrors the
/// blocking path's in-memory cap: once a stream hits this, the reader is
/// dropped, closing the pipe so a runaway producer gets EPIPE and exits
/// rather than filling the disk.
const MAX_BACKGROUND_OUTPUT_BYTES: u64 = 10 * 1024 * 1024;

/// Directory a detached command's output files (`<handle>.out` / `.err`)
/// stream to. Single source of truth for the tool, the notification (which
/// surfaces the path), and the boot-time pruner.
pub fn background_output_dir(workspace_paths: &WorkspacePaths) -> PathBuf {
    workspace_paths.logs_dir().join("background")
}

/// Delete background-command output files older than `max_age`. Called once
/// at boot so completed-but-never-cleaned detached outputs (the agent reads
/// them shortly after the completion notification) don't accumulate forever.
/// Best-effort: unreadable entries are skipped. Returns the count removed.
pub fn prune_background_outputs(dir: &Path, max_age: Duration) -> usize {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::UNIX_EPOCH);
    prune_outputs_before(dir, cutoff)
}

fn prune_outputs_before(dir: &Path, cutoff: std::time::SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut pruned = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file()
            && meta.modified().is_ok_and(|m| m < cutoff)
            && std::fs::remove_file(entry.path()).is_ok()
        {
            pruned += 1;
        }
    }
    pruned
}

enum DetachedOutcome {
    Cancelled,
    Completed(i32),
    Backgrounded,
}

/// Run a command on the detached path: spawn it live, stream stdout/stderr
/// to per-turn files, and wait up to the foreground budget. If it finishes
/// in time, return the normal result; if it overruns, hand the still-running
/// child to the [`BackgroundJobSink`] and return a "moved to background"
/// notice. Returns `Ok(None)` when the command couldn't be detached (sandbox
/// backend without detached support / spawn failure) so the caller falls back
/// to the blocking kill-on-timeout path.
#[allow(clippy::too_many_arguments)]
async fn run_detached(
    tool: &BashTool,
    route: DetachedExecutionRoute,
    command: &str,
    args: &[String],
    cwd: Option<&Path>,
    extra_env: &ChildEnv,
    timeout: Duration,
    ctx: &ToolContext,
    sink: &Arc<dyn BackgroundJobSink>,
    escape_policy: SandboxEscapePolicy,
) -> crate::Result<Option<ToolOutput>> {
    let Some(mut child) = spawn_detached_child(
        &tool.process_manager,
        route,
        args,
        cwd,
        &extra_env.vars,
        timeout,
        ctx,
    )
    .await?
    else {
        return Ok(None);
    };

    // One id shared by the output files, the handle returned to the LLM, and
    // the eventual completion notification.
    let handle_id = new_background_handle();
    let bg_dir = background_output_dir(&ctx.workspace_paths);
    let _ = tokio::fs::create_dir_all(&bg_dir).await;
    let stdout_path = bg_dir.join(format!("{handle_id}.out"));
    let stderr_path = bg_dir.join(format!("{handle_id}.err"));

    // The file IS the capture: a backgrounded command's output isn't bounded
    // by memory, and a completed one is read back below.
    let copy_tasks = vec![
        spawn_copy_to_file(child.take_stdout(), stdout_path.clone()),
        spawn_copy_to_file(child.take_stderr(), stderr_path.clone()),
    ];

    // Wait up to the foreground budget. The wait future is scoped so its
    // `&mut` borrow of `child` ends before the backgrounded arm moves it.
    let outcome = {
        let exit_fut = child.wait();
        tokio::pin!(exit_fut);
        tokio::select! {
            biased;
            _ = ctx.cancellation_token.cancelled() => DetachedOutcome::Cancelled,
            code = &mut exit_fut => DetachedOutcome::Completed(code),
            _ = tokio::time::sleep(timeout) => DetachedOutcome::Backgrounded,
        }
    };

    match outcome {
        DetachedOutcome::Cancelled => {
            child.start_kill();
            let _ = child.wait().await;
            for t in copy_tasks {
                let _ = t.await;
            }
            let _ = tokio::fs::remove_file(&stdout_path).await;
            let _ = tokio::fs::remove_file(&stderr_path).await;
            Err(ToolError::Execution("cancelled".into()))
        }
        DetachedOutcome::Completed(exit_code) => {
            for t in copy_tasks {
                let _ = t.await;
            }
            let stdout = tokio::fs::read(&stdout_path).await.unwrap_or_default();
            let stderr = tokio::fs::read(&stderr_path).await.unwrap_or_default();
            // Transient foreground capture — drop the files now that the
            // result carries the output.
            let _ = tokio::fs::remove_file(&stdout_path).await;
            let _ = tokio::fs::remove_file(&stderr_path).await;
            // A command that completed in-window (the common fast-failing case)
            // still gets the auto-permission on-failure judge — same as the
            // blocking path. A command that overran and backgrounded does not
            // (below).
            let out = crate::SandboxedOutput {
                exit_code,
                stdout,
                stderr,
                timed_out: false,
            };
            let (out, escalation) = tool
                .escalate_if_failed(command, cwd, out, extra_env, timeout, ctx, escape_policy)
                .await?;
            Ok(Some(
                format_command_result(
                    command,
                    out.exit_code,
                    &out.stdout,
                    &out.stderr,
                    extra_env,
                    ctx,
                    escalation.as_deref(),
                )
                .await?,
            ))
        }
        DetachedOutcome::Backgrounded => {
            let display_path = stdout_path.display().to_string();
            let stderr_display_path = stderr_path.display().to_string();
            let secret_risk_note = if extra_env.secret_values.is_empty() {
                ""
            } else {
                tracing::warn!(
                    target: "baybo::tools::bash",
                    command = %command,
                    secret_env_count = extra_env.secret_values.len(),
                    stdout_path = %display_path,
                    stderr_path = %stderr_display_path,
                    "background Bash command injected secret_env; output files are not redacted"
                );
                " Secret environment variables were injected; background output files and completion tails are stored raw and are not secret-redacted."
            };
            let turn = DetachedCommand {
                handle_id: handle_id.clone(),
                session_id: ctx.session_id.clone(),
                command: command.to_string(),
                child,
                copy_tasks,
                stdout_path,
                stderr_path,
            };
            let returned = sink.detach_command(turn).await;
            Ok(Some(ToolOutput::Text(format!(
                "Command still running after {timeout:?}; moved to the background as `{returned}`. \
                 Output is streaming to `{display_path}` (Read it for progress). You'll get a \
                 notification when it finishes — keep working in the meantime.{secret_risk_note}"
            ))))
        }
    }
}

async fn spawn_detached_child(
    process_manager: &Arc<baybo_process::ProcessManager>,
    route: DetachedExecutionRoute,
    args: &[String],
    cwd: Option<&Path>,
    extra_env: &[(String, String)],
    timeout: Duration,
    ctx: &ToolContext,
) -> crate::Result<Option<Box<dyn RunningChild>>> {
    match route {
        DetachedExecutionRoute::Sandboxed => {
            let Some(sandbox) = ctx.sandbox.as_ref() else {
                return Ok(None);
            };
            match sandbox
                .spawn_command_detached(
                    Path::new("sh"),
                    args,
                    SpawnOpts {
                        cwd: cwd.map(Path::to_path_buf),
                        stdin: None,
                        extra_env: extra_env.to_vec(),
                        timeout,
                    },
                )
                .await
            {
                Ok(child) => Ok(Some(child)),
                Err(_) => Ok(None),
            }
        }
        DetachedExecutionRoute::Unsandboxed => {
            match spawn_unsandboxed_detached(process_manager, "sh", args, cwd, extra_env) {
                Ok(child) => Ok(Some(child)),
                Err(_) => Ok(None),
            }
        }
    }
}

/// Stream a child pipe to its output file, capped at
/// [`MAX_BACKGROUND_OUTPUT_BYTES`] so a runaway producer can't fill the disk.
fn spawn_copy_to_file(
    reader: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    path: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Some(reader) = reader else {
            return;
        };
        if let Ok(mut file) = tokio::fs::File::create(&path).await {
            let mut limited = reader.take(MAX_BACKGROUND_OUTPUT_BYTES);
            let _ = tokio::io::copy(&mut limited, &mut file).await;
            let _ = file.flush().await;
        }
    })
}

/// Reject an absolute path that lives inside the workspace root but
/// outside the work directory. Bash invocations are scoped to the
/// `work/` subtree so the agent can't accidentally clobber the
/// gateway's own `config/`, `state/`, `personas/`, `logs/`, or `.key/`
/// subdirectories from a shell call. Paths that are not absolute, or
/// that fall entirely outside the workspace root (FHS roots, `$HOME`,
/// `/tmp`, …) are left to the OS sandbox.
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
             directory. Only `{}` is writable for shell operations (your own \
             skill directory is bound read-only so installed skill scripts can \
             run in place) — move the action under `{}/` or use \
             Read/Edit/Write for the read-only workspace subtrees (personas/, \
             config/, state/, logs/, .key/).",
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
///
/// `skill_root` is exempt: the calling agent's skill directory is bound
/// read-only into the sandbox (see `args.rs`), so naming an installed
/// skill's script there is exactly how a skill is meant to run. Writes
/// still fail at the RO bind, so there's nothing to guard against by
/// rejecting the path token.
fn require_command_paths_within_work_dir(
    command: &str,
    workspace_root: &Path,
    work_dir: &Path,
    skill_root: &Path,
) -> crate::Result<()> {
    for sub in split_into_subcommands(command) {
        for tok in sub {
            let Ok(words) = shell_words::split(tok) else {
                continue;
            };
            for word in words {
                let p = Path::new(&word);
                if p.is_absolute() && p.starts_with(skill_root) {
                    continue;
                }
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

const FILE_TOOL_REDIRECT_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "tac", "sed", "awk", "gawk", "mawk",
];

fn is_file_tool_redirect(command: &str) -> bool {
    first_token(command).is_some_and(|t| FILE_TOOL_REDIRECT_COMMANDS.contains(&t))
}

/// Prefix `command` with `export BAYBO_HELP_AGENT=1; export
/// BAYBO_CONFIG_PATH=…;` when the command contains the cargo bin name
/// (`baybo_workspace::paths::BIN_NAME`). This gives the subshell two
/// things the agent would otherwise be missing:
///
/// 1. The extended-help inventory (hidden subcommands like `cost`,
///    `log`, `session`, `turn`, `cron`, `config`). See
///    `baybo_cli::cli::ENV_HELP_AGENT` for the reader contract.
/// 2. The same config file the running gateway is using. Reads
///    `BAYBO_CONFIG_PATH` from the parent process when set, falls
///    back to [`baybo_workspace::paths::default_config_file`]
///    otherwise. The path is always resolved to an absolute form so
///    a relative debug-mode default (`./.baybo/config/baybo.json`)
///    keeps pointing at the right workspace even when the bash tool
///    spawns the child with a different cwd.
///
/// The substring match is intentionally loose: non-baybo processes
/// inherit the variables and ignore them, so a false-positive
/// injection (e.g. `cd /data/baybo && cargo build`) has no observable
/// effect. The win is that the agent can compose `baybo …` commands
/// naturally — no per-call argv token, no LLM tool-shape change.
fn inject_baybo_env(command: &str) -> String {
    let raw = std::env::var_os(baybo_workspace::paths::ENV_CONFIG_PATH)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(baybo_workspace::paths::default_config_file);
    // Syntactic absolutize — fast, doesn't touch the FS, doesn't
    // fail when the file is missing (which is the normal case in
    // fresh deployments before `baybo setup` runs).
    let abs = std::path::absolute(&raw).unwrap_or(raw);
    inject_baybo_env_with(command, abs.as_os_str())
}

/// Pure variant of [`inject_baybo_env`] that takes an already-resolved
/// config path. Split out so tests don't have to mutate process env.
///
/// The "does this command invoke the CLI" check is a substring match
/// against `baybo_workspace::paths::BIN_NAME` — that const is the
/// single source of truth for the cargo `[[bin]]` name, so renaming
/// the binary changes the trigger token automatically.
fn inject_baybo_env_with(command: &str, config_path: &std::ffi::OsStr) -> String {
    if !command.contains(baybo_workspace::paths::BIN_NAME) {
        return command.to_string();
    }
    format!(
        "export BAYBO_HELP_AGENT=1; export {}={}; {command}",
        baybo_workspace::paths::ENV_CONFIG_PATH,
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

/// Canonical absolute path of the running gateway binary, cached on
/// first read. Drives the baybo self-CLI match in
/// [`classify_baybo_cli_command`]: path-like argv0s (`/usr/local/bin/baybo`,
/// `./target/debug/baybo`) are compared against THIS path, not against
/// the literal string `"baybo"`, so an unrelated binary that happens
/// to be named `baybo` somewhere else on disk does NOT run unsandboxed.
///
/// Falls back to the raw `current_exe()` path if `canonicalize` fails
/// (binary deleted post-exec, etc.); returns `None` only if
/// `current_exe()` itself errors, which is rare enough that we treat
/// it as "no baybo CLI is locatable, sandbox every command".
static BAYBO_BIN: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let exe = std::env::current_exe().ok()?;
    Some(std::fs::canonicalize(&exe).unwrap_or(exe))
});

/// What the command line appears to be relative to the gateway's own `baybo`
/// CLI. This enum deliberately does not mention sandboxing; the final execution
/// route also depends on [`BashPermissionMode`].
///
/// The sandbox masks the Baybo state dir (`~/.baybo`/`$BAYBO_HOME`), so a
/// sandboxed `baybo …` can't reach the gateway's config or session
/// store. That makes the self-CLI classification worth getting right in three
/// directions:
///
/// - [`CanonicalSelfInvocation`](BayboCliCommandKind::CanonicalSelfInvocation):
///   the command starts with the canonical absolute path of the gateway binary.
/// - [`NonCanonicalSelfInvocation`](BayboCliCommandKind::NonCanonicalSelfInvocation):
///   the command is clearly trying to invoke baybo (its argv0's `file_name`
///   matches the gateway binary), but the caller used a bare/relative/wrong
///   absolute path.
/// - [`OtherCommand`](BayboCliCommandKind::OtherCommand): baybo isn't the
///   leading sub-command, OR an unsafe-env shape prevents treating it as a
///   trusted self-invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BayboCliCommandKind {
    CanonicalSelfInvocation,
    NonCanonicalSelfInvocation,
    OtherCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BashExecutionRoute {
    RejectNonCanonicalBayboCliPath,
    RunBayboCliUnsandboxed,
    RunUnsandboxed,
    RunSandboxed {
        pre_exec_judge: bool,
        escape_policy: SandboxEscapePolicy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachedExecutionRoute {
    Sandboxed,
    Unsandboxed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxEscapePolicy {
    None,
    AutoJudge,
    ManualApproval,
}

enum SandboxEscapeDecision {
    /// The failure was not the sandbox's doing: surface it unchanged, without
    /// offering an escape the user has no reason to grant.
    Keep,
    Run(String),
    Prompt(String),
}

impl BashExecutionRoute {
    fn is_sandboxed(self) -> bool {
        matches!(self, BashExecutionRoute::RunSandboxed { .. })
    }

    fn is_unsandboxed(self) -> bool {
        matches!(
            self,
            BashExecutionRoute::RunBayboCliUnsandboxed | BashExecutionRoute::RunUnsandboxed
        )
    }

    fn detached_route(self) -> Option<DetachedExecutionRoute> {
        match self {
            BashExecutionRoute::RunSandboxed { .. } => Some(DetachedExecutionRoute::Sandboxed),
            BashExecutionRoute::RunBayboCliUnsandboxed | BashExecutionRoute::RunUnsandboxed => {
                Some(DetachedExecutionRoute::Unsandboxed)
            }
            BashExecutionRoute::RejectNonCanonicalBayboCliPath => None,
        }
    }

    fn pre_exec_judge(self) -> bool {
        matches!(
            self,
            BashExecutionRoute::RunSandboxed {
                pre_exec_judge: true,
                ..
            }
        )
    }

    fn escape_policy(self) -> SandboxEscapePolicy {
        match self {
            BashExecutionRoute::RunSandboxed { escape_policy, .. } => escape_policy,
            _ => SandboxEscapePolicy::None,
        }
    }
}

fn bash_execution_route(command: &str, permission: BashPermissionMode) -> BashExecutionRoute {
    bash_execution_route_for_kind(classify_baybo_cli_command(command), permission)
}

#[cfg(test)]
fn bash_execution_route_with_bin(
    command: &str,
    bin: &Path,
    permission: BashPermissionMode,
) -> BashExecutionRoute {
    bash_execution_route_for_kind(
        classify_baybo_cli_command_with_bin(command, bin),
        permission,
    )
}

fn bash_execution_route_for_kind(
    kind: BayboCliCommandKind,
    permission: BashPermissionMode,
) -> BashExecutionRoute {
    match kind {
        BayboCliCommandKind::CanonicalSelfInvocation => BashExecutionRoute::RunBayboCliUnsandboxed,
        BayboCliCommandKind::NonCanonicalSelfInvocation => {
            BashExecutionRoute::RejectNonCanonicalBayboCliPath
        }
        BayboCliCommandKind::OtherCommand if permission_skips_os_sandbox(permission) => {
            BashExecutionRoute::RunUnsandboxed
        }
        BayboCliCommandKind::OtherCommand => match permission {
            BashPermissionMode::Auto => BashExecutionRoute::RunSandboxed {
                pre_exec_judge: true,
                escape_policy: SandboxEscapePolicy::AutoJudge,
            },
            BashPermissionMode::Manual => BashExecutionRoute::RunSandboxed {
                pre_exec_judge: false,
                escape_policy: SandboxEscapePolicy::ManualApproval,
            },
            BashPermissionMode::Free => BashExecutionRoute::RunUnsandboxed,
        },
    }
}

fn classify_baybo_cli_command(command: &str) -> BayboCliCommandKind {
    let Some(bin) = BAYBO_BIN.as_deref() else {
        return BayboCliCommandKind::OtherCommand;
    };
    classify_baybo_cli_command_with_bin(command, bin)
}

fn classify_baybo_cli_command_with_bin(command: &str, bin: &Path) -> BayboCliCommandKind {
    // Only the FIRST sub-command's argv0 matters: if the user opens
    // the command line with an absolute-path baybo invocation, the
    // whole `sh -c` string runs unsandboxed (compound forms like
    // `baybo … && cat /etc/passwd`, `baybo … | jq`, `$(baybo …)`
    // included). A non-baybo leader keeps the sandbox.
    let subs = split_into_subcommands(command);
    let Some(tokens) = subs.first() else {
        return BayboCliCommandKind::OtherCommand;
    };

    let mut unquoted: Vec<String> = Vec::with_capacity(tokens.len());
    for tok in tokens {
        match shell_words::split(tok) {
            Ok(mut words) if words.len() <= 1 => {
                unquoted.push(words.pop().unwrap_or_default());
            }
            _ => return BayboCliCommandKind::OtherCommand,
        }
    }
    let mut i = 0;
    while let Some(tok) = unquoted.get(i) {
        if !is_env_assignment(tok) {
            break;
        }
        if !is_safe_baybo_env_assignment(tok) {
            return BayboCliCommandKind::OtherCommand;
        }
        i += 1;
    }
    let Some(argv0) = unquoted.get(i) else {
        return BayboCliCommandKind::OtherCommand;
    };

    // "Looks like baybo" — basename of argv0 matches the gateway
    // binary's basename. Catches bare `baybo`, relative `./baybo`,
    // and wrong absolute paths (`/opt/imposter/baybo`) — every form
    // where the caller appears to be trying to spawn the baybo CLI.
    let argv0_filename = Path::new(argv0).file_name();
    let bin_filename = bin.file_name();
    let looks_like_baybo = match (argv0_filename, bin_filename) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if !looks_like_baybo {
        return BayboCliCommandKind::OtherCommand;
    }

    if argv0_matches_gateway_binary(argv0, bin) {
        BayboCliCommandKind::CanonicalSelfInvocation
    } else {
        BayboCliCommandKind::NonCanonicalSelfInvocation
    }
}

/// Env assignments allowed as a prefix on a baybo invocation without forfeiting
/// the unsandboxed route. The whitelist is intentionally narrow:
/// the `BAYBO_` family (gateway-owned config the CLI reads) and the two
/// `RUST_*` knobs the agent commonly uses to surface tracing. Anything
/// else — `PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*`,
/// `HOME`/`XDG_*`, locale vars, … — could redirect command resolution
/// or library loading and so prevents the trusted self-invocation match.
fn is_safe_baybo_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else {
        return false;
    };
    let key = &tok[..eq];
    key.starts_with("BAYBO_") || matches!(key, "RUST_LOG" | "RUST_BACKTRACE")
}

/// True when `argv0` is a literal absolute path that resolves to the
/// same FS object as `bin`. Bare names and relative paths are
/// rejected outright — the unsandboxed route requires the caller to have
/// spelled out the absolute path, so the unsandboxed shell's `execve` never
/// consults `$PATH` or the current working directory.
fn argv0_matches_gateway_binary(argv0: &str, bin: &Path) -> bool {
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

impl BashTool {
    #[allow(clippy::too_many_arguments)]
    async fn handle_sandbox_start_failure(
        &self,
        command: &str,
        cwd: Option<&Path>,
        extra_env: &ChildEnv,
        timeout: Duration,
        ctx: &ToolContext,
        sandbox_err: ToolError,
        policy: SandboxEscapePolicy,
    ) -> crate::Result<crate::SandboxedOutput> {
        let reason = sandbox_err.to_string();
        let decision = match policy {
            SandboxEscapePolicy::AutoJudge => {
                self.auto_sandbox_escape_decision(command, cwd, -1, "", &reason, extra_env, ctx)
                    .await?
            }
            SandboxEscapePolicy::ManualApproval => SandboxEscapeDecision::Prompt(reason.clone()),
            SandboxEscapePolicy::None => {
                return Err(ToolError::Execution(format!(
                    "sandboxed run failed and sandbox escape is disabled: {sandbox_err}"
                )));
            }
        };

        let rationale = match decision {
            SandboxEscapeDecision::Run(rationale) => {
                self.notify_escape(ctx, command, &rationale);
                return self
                    .run_unsandboxed_wrapped(command, cwd, &extra_env.vars, timeout, ctx)
                    .await;
            }
            SandboxEscapeDecision::Prompt(rationale) => rationale,
            // The sandbox never started, so the failure is sandbox-caused by
            // construction — the judge's `sandbox_related` half carries no
            // information here, only its risk verdict does. Still ask.
            SandboxEscapeDecision::Keep => reason.clone(),
        };

        if self
            .request_unsandboxed_retry_approval(command, ctx, &rationale, Some(&reason))
            .await?
        {
            self.notify_escape(ctx, command, &rationale);
            self.run_unsandboxed_wrapped(command, cwd, &extra_env.vars, timeout, ctx)
                .await
        } else {
            Err(ToolError::Execution(format!(
                "sandboxed run failed and the unsandboxed retry was not approved: {sandbox_err}"
            )))
        }
    }

    /// Run `command` unsandboxed via `sh -c` (with the workspace uv/env wrap),
    /// honoring the cancellation token. Shared by the sandbox-failure retry and
    /// the auto-permission escalation so both compose the `sh -c` body
    /// identically.
    async fn run_unsandboxed_wrapped(
        &self,
        command: &str,
        cwd: Option<&Path>,
        extra_env: &[(String, String)],
        timeout: Duration,
        ctx: &ToolContext,
    ) -> crate::Result<crate::SandboxedOutput> {
        let args = ["-c".to_string(), self.wrap_command(command)];
        tokio::select! {
            _ = ctx.cancellation_token.cancelled() => {
                Err(ToolError::Execution("cancelled".into()))
            }
            res = run_unsandboxed(&self.process_manager, "sh", &args, cwd, extra_env, timeout) => res,
        }
    }

    /// Auto-permission pre-execution gate for a destructive-token command: the LLM
    /// judge decides whether it needs human approval before running (sandboxed).
    /// `Ok(())` proceeds; `Err` aborts (denied / no approval channel). Uses the
    /// cached approval path so an "approve always" sticks like the legacy gate.
    async fn pre_exec_gate(
        &self,
        command: &str,
        cwd: Option<&Path>,
        ctx: &ToolContext,
        sandboxed: bool,
    ) -> crate::Result<()> {
        let decision = match ctx.lite_llm.as_deref() {
            Some(llm) => judge_pre_exec(llm, &ctx.events, command, cwd, sandboxed).await,
            // No judge wired (argv mode / tests): fall back to requiring
            // approval, matching the non-auto destructive gate.
            None => PreExec::Prompt("risk judge unavailable — approval required".to_string()),
        };
        let rationale = match decision {
            PreExec::Proceed => return Ok(()),
            PreExec::Prompt(rationale) => rationale,
        };
        let Some(approval) = ctx.approval.as_ref() else {
            return Err(ToolError::Execution(format!(
                "destructive command requires approval but no approval channel is available \
                 ({rationale})"
            )));
        };
        let run_location = if sandboxed {
            "inside the OS sandbox"
        } else {
            "without the OS sandbox"
        };
        let preview = format!(
            "Destructive `Bash` command flagged by the risk judge.\n\
             Command : {command}\n\
             Reason  : {rationale}\n\
             Approve to run it ({run_location})."
        );
        let decision = approval
            .request(
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
            ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => Ok(()),
            ApprovalDecision::Deny => Err(ToolError::Execution(format!(
                "destructive command denied ({rationale})"
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn auto_sandbox_escape_decision(
        &self,
        command: &str,
        cwd: Option<&Path>,
        exit_code: i32,
        stdout: &str,
        stderr: &str,
        extra_env: &ChildEnv,
        ctx: &ToolContext,
    ) -> crate::Result<SandboxEscapeDecision> {
        let Some(llm) = ctx.lite_llm.as_deref() else {
            return Ok(SandboxEscapeDecision::Prompt(
                "risk judge unavailable — approval required for sandbox escape".to_string(),
            ));
        };

        // Two passes, neither subsuming the other: `redact` removes the values
        // this run injected as env vars (which the detector has no pattern
        // for), `sanitize` runs the detector over whatever else was printed.
        // Gating the second on `extra_env` would ship an ordinary command's
        // capture to the judge verbatim.
        let mut stdout_s = stdout.to_string();
        let mut stderr_s = stderr.to_string();
        if let Some(handle) = ctx.secrets.as_deref() {
            if !extra_env.secret_values.is_empty() {
                stdout_s = handle.redact(&stdout_s, &extra_env.secret_values).await?;
                stderr_s = handle.redact(&stderr_s, &extra_env.secret_values).await?;
            }
            stdout_s = handle.sanitize(&stdout_s).await?;
            stderr_s = handle.sanitize(&stderr_s).await?;
        }

        match judge_post_fail(
            llm,
            &ctx.events,
            command,
            cwd,
            exit_code,
            &stdout_s,
            &stderr_s,
        )
        .await
        {
            PostFail::Unsandbox(rationale) => Ok(SandboxEscapeDecision::Run(rationale)),
            PostFail::Prompt(rationale) => Ok(SandboxEscapeDecision::Prompt(rationale)),
            PostFail::Keep => Ok(SandboxEscapeDecision::Keep),
        }
    }

    async fn request_unsandboxed_retry_approval(
        &self,
        command: &str,
        ctx: &ToolContext,
        rationale: &str,
        failure: Option<&str>,
    ) -> crate::Result<bool> {
        let Some(approval) = ctx.approval.as_ref() else {
            return Ok(false);
        };
        let failure_line = failure
            .map(|f| format!("Failure : {f}\n"))
            .unwrap_or_default();
        let preview = format!(
            "Sandboxed `Bash` command failed.\n\
             Command : {command}\n\
             {failure_line}\
             Reason  : {rationale}\n\
             Approve to retry WITHOUT the OS sandbox (full shell, no workspace guard)."
        );
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
        Ok(matches!(
            decision,
            ApprovalDecision::Approve | ApprovalDecision::ApproveAlways
        ))
    }

    /// On sandbox failure, decide whether to retry unsandboxed. Auto asks the
    /// risk judge first: a failure the judge ties to the sandbox either re-runs
    /// outright (safe) or falls through to human approval (risky), while an
    /// ordinary failure the sandbox had no part in is returned untouched — the
    /// escape prompt is the most privileged one in the system, so it must not
    /// fire on every non-zero exit. Manual goes straight to approval. Returns
    /// the (possibly re-run) output plus an optional note for the tool result.
    #[allow(clippy::too_many_arguments)]
    async fn escalate_if_failed(
        &self,
        command: &str,
        cwd: Option<&Path>,
        out: crate::SandboxedOutput,
        extra_env: &ChildEnv,
        timeout: Duration,
        ctx: &ToolContext,
        policy: SandboxEscapePolicy,
    ) -> crate::Result<(crate::SandboxedOutput, Option<String>)> {
        if policy == SandboxEscapePolicy::None || out.exit_code == 0 || out.timed_out {
            return Ok((out, None));
        }

        let decision = match policy {
            SandboxEscapePolicy::AutoJudge => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                self.auto_sandbox_escape_decision(
                    command,
                    cwd,
                    out.exit_code,
                    &stdout,
                    &stderr,
                    extra_env,
                    ctx,
                )
                .await?
            }
            SandboxEscapePolicy::ManualApproval => SandboxEscapeDecision::Prompt(format!(
                "sandboxed command failed with exit code {}",
                out.exit_code
            )),
            SandboxEscapePolicy::None => return Ok((out, None)),
        };

        match decision {
            SandboxEscapeDecision::Keep => Ok((out, None)),
            SandboxEscapeDecision::Run(rationale) => {
                self.notify_escape(ctx, command, &rationale);
                let new = self
                    .run_unsandboxed_wrapped(command, cwd, &extra_env.vars, timeout, ctx)
                    .await?;
                Ok((
                    new,
                    Some(format!(
                        "ran outside the OS sandbox after risk check: {rationale}"
                    )),
                ))
            }
            SandboxEscapeDecision::Prompt(rationale) => {
                let failure = format!("exit code {}", out.exit_code);
                if self
                    .request_unsandboxed_retry_approval(command, ctx, &rationale, Some(&failure))
                    .await?
                {
                    self.notify_escape(ctx, command, &rationale);
                    let new = self
                        .run_unsandboxed_wrapped(command, cwd, &extra_env.vars, timeout, ctx)
                        .await?;
                    Ok((
                        new,
                        Some(format!(
                            "ran outside the OS sandbox after user approval: {rationale}"
                        )),
                    ))
                } else {
                    Ok((out, None))
                }
            }
        }
    }

    fn notify_sandbox_bypass(&self, ctx: &ToolContext, command: &str) {
        let Some(reason) = ctx.sandbox_bypass_reason.as_deref() else {
            tracing::debug!(
                target: "baybo::tools::bash",
                command_head = %command_head(command),
                command_len = command.len(),
                "running Bash without the inner OS sandbox; user notice suppressed"
            );
            return;
        };
        tracing::warn!(
            target: "baybo::tools::bash",
            command_head = %command_head(command),
            command_len = command.len(),
            reason = %reason,
            "running Bash without the inner OS sandbox"
        );
        if let Some(notifier) = ctx.notifier.as_ref() {
            notifier.emit(
                NoticeLevel::Warn,
                "Bash is running without the OS sandbox",
                &format!("{reason}\nCommand: {command}"),
            );
        }
    }

    /// Surface an unsandboxed retry to the user channel (no-op in cron / tests)
    /// and the structured log, so a sandbox escape is never silent.
    fn notify_escape(&self, ctx: &ToolContext, command: &str, rationale: &str) {
        tracing::warn!(
            target: "baybo::tools::bash",
            command_head = %command_head(command),
            command_len = command.len(),
            rationale = %rationale,
            "running a failed command outside the OS sandbox"
        );
        if let Some(notifier) = ctx.notifier.as_ref() {
            notifier.emit(
                NoticeLevel::Warn,
                "Ran a command outside the sandbox",
                &format!("{command}\n— {rationale}"),
            );
        }
    }
}

struct ProcessGroupRunningChild {
    child: baybo_process::ManagedChild,
}

#[async_trait]
impl RunningChild for ProcessGroupRunningChild {
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.child
            .take_stdout()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.child
            .take_stderr()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }

    async fn wait(&mut self) -> i32 {
        self.child
            .wait()
            .await
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1)
    }

    fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn build_unsandboxed_command(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    extra_env: &[(String, String)],
) -> tokio::process::Command {
    use std::process::Stdio;
    use tokio::process::Command;

    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        cmd.env("PWD", dir);
    }
    // Inject resolved secrets as real env vars on the child only. The
    // unsandboxed child inherits the parent env, so these add/override keys.
    cmd.envs(extra_env.iter().cloned());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Spawn a detached child in its own process group.
///
/// `pub(crate)` for [`crate::test_support::FakeExecSandbox`], which stands in
/// for a sandbox backend by running the command directly: routing it through
/// this function is what makes the fake's detached child behave like a real
/// one — killable as a whole tree, and carrying a pgid to record.
pub(crate) fn spawn_unsandboxed_detached(
    process_manager: &Arc<baybo_process::ProcessManager>,
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    extra_env: &[(String, String)],
) -> crate::Result<Box<dyn RunningChild>> {
    let mut command = build_unsandboxed_command(program, args, cwd, extra_env);
    let child = process_manager
        .spawn(&mut command, format!("bash-detached:{program}"))
        .map_err(|e| ToolError::Execution(format!("spawn `{program}`: {e}")))?;
    Ok(Box::new(ProcessGroupRunningChild { child }))
}

async fn run_unsandboxed(
    process_manager: &Arc<baybo_process::ProcessManager>,
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    extra_env: &[(String, String)],
    timeout: Duration,
) -> crate::Result<crate::SandboxedOutput> {
    use tokio::io::AsyncReadExt;

    let mut command = build_unsandboxed_command(program, args, cwd, extra_env);
    let mut child = process_manager
        .spawn(&mut command, format!("bash:{program}"))
        .map_err(|e| ToolError::Execution(format!("spawn `{program}`: {e}")))?;

    let stdout_pipe = child
        .take_stdout()
        .ok_or_else(|| ToolError::Execution("child stdout pipe missing".into()))?;
    let stderr_pipe = child
        .take_stderr()
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
    use baybo_model::{ChannelType, User};
    use parking_lot::Mutex;

    /// A phrase only the sandboxed `{{isolation}}` section carries, so a test
    /// can tell "the OS sandbox is on" from `free`'s "no credential-vault
    /// masking" without matching both.
    const SANDBOXED_MARKER: &str = "are masked and read as empty";
    use std::sync::Arc;

    fn cfg(path: &str) -> std::ffi::OsString {
        std::ffi::OsString::from(path)
    }

    /// A bash command that forks a long-lived descendant must have that whole
    /// tree reaped when it times out — not just the `sh` we launched. Regression
    /// guard for the orphaned-process-group leak (descendants reparented to init
    /// and left spinning) that `process_group(0)` + group-SIGKILL fixes.
    #[tokio::test]
    async fn unsandboxed_timeout_reaps_whole_process_group() {
        let pidfile = std::env::temp_dir().join(format!(
            "baybo-pgkill-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&pidfile);

        // `sleep 30` is the grandchild (baybo -> sh -> sleep); it inherits sh's
        // new process group. `wait` keeps sh alive so the command hits the
        // timeout instead of exiting on its own.
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let process_manager = baybo_process::ProcessManager::transient();
        let out = run_unsandboxed(
            &process_manager,
            "sh",
            &["-c".to_string(), script],
            None,
            &[],
            std::time::Duration::from_millis(300),
        )
        .await
        .expect("run_unsandboxed returns");
        assert!(out.timed_out, "command should have hit the timeout");

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pidfile written")
            .trim()
            .parse()
            .expect("pid parses");
        let _ = std::fs::remove_file(&pidfile);

        // `kill(pid, 0)` returns -1/ESRCH once the grandchild is gone. Poll
        // briefly for the SIGKILL to land.
        let reaped = wait_until_pid_gone(pid).await;
        if !reaped {
            // Don't leak the survivor if the fix regressed.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(
            reaped,
            "grandchild pid {pid} survived the timeout — process group not reaped"
        );
    }

    async fn wait_until_pid_gone(pid: i32) -> bool {
        for _ in 0..40 {
            if unsafe { libc::kill(pid, 0) != 0 } {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    #[test]
    fn inject_baybo_env_prefixes_baybo_commands() {
        let c = cfg("/data/baybo/baybo.json");
        let out = inject_baybo_env_with("baybo cost show", c.as_os_str());
        assert!(
            out.starts_with("export BAYBO_HELP_AGENT=1; "),
            "expected BAYBO_HELP_AGENT export prefix, got: {out}"
        );
        assert!(
            out.contains("export BAYBO_CONFIG_PATH='/data/baybo/baybo.json'"),
            "expected config-path export, got: {out}"
        );
        assert!(out.ends_with("; baybo cost show"));
    }

    #[test]
    fn inject_baybo_env_quotes_config_path_with_spaces_and_quotes() {
        // Path with a space + an embedded single quote — the latter
        // is rare on disk but the escape path must still work.
        let c = cfg("/tmp/baybo's space/baybo.json");
        let out = inject_baybo_env_with("baybo doctor", c.as_os_str());
        assert!(
            out.contains("export BAYBO_CONFIG_PATH='/tmp/baybo'\\''s space/baybo.json'"),
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
    fn inject_baybo_env_leaves_unrelated_commands_alone() {
        let c = cfg("/x/baybo.json");
        assert_eq!(inject_baybo_env_with("ls -la", c.as_os_str()), "ls -la");
        assert_eq!(
            inject_baybo_env_with("git status", c.as_os_str()),
            "git status"
        );
    }

    #[test]
    fn inject_baybo_env_triggers_inside_pipelines_and_chains() {
        let c = cfg("/x/baybo.json");
        for cmd in [
            "baybo status --live | jq .",
            "cd /tmp && baybo cost show",
            "for i in 1 2; do baybo turn list; done",
        ] {
            let out = inject_baybo_env_with(cmd, c.as_os_str());
            assert!(
                out.starts_with("export BAYBO_HELP_AGENT=1; "),
                "expected env prefix for {cmd:?}, got: {out}"
            );
            assert!(
                out.contains("export BAYBO_CONFIG_PATH="),
                "expected config-path export for {cmd:?}, got: {out}"
            );
        }
    }

    #[test]
    fn inject_baybo_env_falls_back_to_default_config_when_env_unset() {
        // Exercises the public wrapper rather than the pure helper —
        // verifies that an unset `BAYBO_CONFIG_PATH` still produces an
        // export pointing at the workspace default. We can't safely
        // mutate process env in a parallel test, so we settle for
        // asserting the export is present + absolute.
        // SAFETY: this test runs in the bash unit-test module; tokio
        // is not initialized, no concurrent reader is observing the
        // var while we mutate it.
        unsafe {
            std::env::remove_var(baybo_workspace::paths::ENV_CONFIG_PATH);
        }
        let out = inject_baybo_env("baybo status");
        assert!(
            out.contains("export BAYBO_CONFIG_PATH='"),
            "default config should still be exported, got: {out}"
        );
        // The default workspace root is absolute in release and
        // resolves to absolute via `std::path::absolute` in debug —
        // either way the exported path starts with `/`.
        let after_eq = out
            .split("BAYBO_CONFIG_PATH='")
            .nth(1)
            .expect("export present");
        assert!(
            after_eq.starts_with('/'),
            "exported path should be absolute, got: {after_eq}"
        );
    }

    #[test]
    fn build_uv_env_exports_points_at_workspace_subdirs() {
        let paths = WorkspacePaths::new("/var/baybo");
        let prefix = build_uv_env_exports(&paths, None);
        assert!(
            prefix.contains("export UV_CACHE_DIR='/var/baybo/work/.uv/cache'"),
            "UV_CACHE_DIR missing or wrong, got: {prefix}",
        );
        assert!(
            prefix.contains("export UV_PYTHON_INSTALL_DIR='/var/baybo/work/.uv/python'"),
            "UV_PYTHON_INSTALL_DIR missing or wrong, got: {prefix}",
        );
        assert!(
            prefix.contains("export UV_TOOL_DIR='/var/baybo/work/.uv/tools'"),
            "UV_TOOL_DIR missing or wrong, got: {prefix}",
        );
        assert!(
            prefix.contains("export UV_TOOL_BIN_DIR='/var/baybo/work/.uv/bin'"),
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
        let paths = WorkspacePaths::new("/tmp/baybo's space");
        let prefix = build_uv_env_exports(&paths, None);
        assert!(
            prefix.contains("export UV_CACHE_DIR='/tmp/baybo'\\''s space/work/.uv/cache'"),
            "UV_CACHE_DIR must be POSIX-quoted, got: {prefix}",
        );
    }

    #[test]
    fn build_uv_env_exports_prepends_resolved_uv_dir_to_path() {
        let paths = WorkspacePaths::new("/var/baybo");
        let prefix = build_uv_env_exports(&paths, Some(Path::new("/home/u/.local/bin")));
        assert!(
            prefix.contains(r#"export PATH='/home/u/.local/bin':"$PATH"; "#),
            "uv dir must be folded onto PATH ahead of the sandbox default, got: {prefix}",
        );
        // PATH export precedes the UV_* exports so the shims resolve `uv`.
        let path_at = prefix.find("export PATH=").expect("PATH export present");
        let cache_at = prefix.find("export UV_CACHE_DIR=").expect("cache export");
        assert!(
            path_at < cache_at,
            "PATH export must come first, got: {prefix}"
        );
        assert!(prefix.contains("python3() { uv run python \"$@\"; }"));
    }

    #[test]
    fn build_uv_env_exports_omits_path_when_uv_absent() {
        let paths = WorkspacePaths::new("/var/baybo");
        let prefix = build_uv_env_exports(&paths, None);
        assert!(
            !prefix.contains("export PATH="),
            "no PATH export when uv is unresolved, got: {prefix}",
        );
    }

    #[test]
    fn uv_bin_dir_in_finds_executable_skipping_empty_and_nonexec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let uv = dir.path().join("uv");
        std::fs::write(&uv, b"#!/bin/sh\n").expect("write uv");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755))
                .expect("chmod uv");
        }
        // Leading empty (implicit-cwd) entry and a non-matching dir are
        // skipped; the executable in `dir` wins.
        let path = std::env::join_paths(["".as_ref(), Path::new("/nonexistent"), dir.path()])
            .expect("join paths");
        assert_eq!(uv_bin_dir_in(&path).as_deref(), Some(dir.path()));
    }

    #[test]
    fn uv_bin_dir_in_rejects_non_executable_uv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let uv = dir.path().join("uv");
        std::fs::write(&uv, b"not exec\n").expect("write uv");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o644))
                .expect("chmod uv");
        }
        let path = std::env::join_paths([dir.path()]).expect("join paths");
        assert_eq!(uv_bin_dir_in(&path), None);
    }

    fn classify(command: &str, bin: &Path) -> BayboCliCommandKind {
        classify_baybo_cli_command_with_bin(command, bin)
    }

    fn auto_permission() -> BashPermissionMode {
        BashPermissionMode::Auto
    }

    fn route(command: &str, bin: &Path, permission: BashPermissionMode) -> BashExecutionRoute {
        bash_execution_route_with_bin(command, bin, permission)
    }

    #[test]
    fn execution_route_combines_baybo_cli_match_with_permission() {
        let bin = Path::new("/usr/local/bin/baybo");
        assert_eq!(
            route("/usr/local/bin/baybo cost", bin, BashPermissionMode::Auto,),
            BashExecutionRoute::RunBayboCliUnsandboxed,
        );
        assert_eq!(
            route("baybo cost", bin, BashPermissionMode::Auto,),
            BashExecutionRoute::RejectNonCanonicalBayboCliPath,
        );
        assert_eq!(
            route("ls -la", bin, BashPermissionMode::Free,),
            BashExecutionRoute::RunUnsandboxed,
        );
        assert_eq!(
            route("ls -la", bin, BashPermissionMode::Manual,),
            BashExecutionRoute::RunSandboxed {
                pre_exec_judge: false,
                escape_policy: SandboxEscapePolicy::ManualApproval,
            },
        );
        if BENCH {
            assert_eq!(
                route("ls -la", bin, auto_permission()),
                BashExecutionRoute::RunUnsandboxed,
            );
        } else {
            assert_eq!(
                route("ls -la", bin, BashPermissionMode::Manual,),
                BashExecutionRoute::RunSandboxed {
                    pre_exec_judge: false,
                    escape_policy: SandboxEscapePolicy::ManualApproval,
                },
            );
            assert_eq!(
                route("ls -la", bin, auto_permission()),
                BashExecutionRoute::RunSandboxed {
                    pre_exec_judge: true,
                    escape_policy: SandboxEscapePolicy::AutoJudge,
                },
            );
            assert_eq!(
                route("ls -la", bin, BashPermissionMode::Free,),
                BashExecutionRoute::RunUnsandboxed,
            );
        }
    }

    #[test]
    fn classify_baybo_marks_only_absolute_canonical_path_as_self_invocation() {
        // Core invariant: ONLY a literal absolute-path argv0 that
        // canonicalises to the gateway binary is a trusted self-invocation.
        let bin = Path::new("/usr/local/bin/baybo");
        assert!(matches!(
            classify("/usr/local/bin/baybo cost", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("/usr/local/bin/baybo", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("/usr/local/bin/baybo status --live", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        // Quoted forms still match — shell_words::split strips the
        // wrapping single quotes before the canonical compare.
        assert!(matches!(
            classify("'/usr/local/bin/baybo' cost", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        // Whitelisted env prefixes preserve the self-invocation match.
        assert!(matches!(
            classify("BAYBO_LOG=trace /usr/local/bin/baybo log", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify(
                "BAYBO_LOG=trace BAYBO_HOME=/x /usr/local/bin/baybo status",
                bin
            ),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("RUST_LOG=debug /usr/local/bin/baybo status", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
    }

    #[test]
    fn classify_baybo_demands_absolute_path_for_bare_or_relative_argv0() {
        // The user-asked behaviour: anything that LOOKS like an baybo
        // invocation (basename match) but isn't spelled out as an
        // absolute path must error rather than silently sandbox.
        // BashTool::execute surfaces this as `InvalidParams` so the
        // agent self-corrects to the canonical absolute path.
        let bin = Path::new("/usr/local/bin/baybo");
        assert!(matches!(
            classify("baybo", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("baybo cost", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("baybo status --live", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
        // Relative path forms still look like baybo but aren't
        // absolute → require absolute path.
        assert!(matches!(
            classify("./baybo cost", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
        // Quoted bare name normalises to bare `baybo`.
        assert!(matches!(
            classify("'baybo' cost", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
        // Whitelisted env + bare argv0 — still require absolute
        // path; safe env doesn't excuse the missing path.
        assert!(matches!(
            classify("BAYBO_LOG=trace baybo log", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
    }

    #[test]
    fn classify_baybo_demands_absolute_path_for_wrong_absolute_path() {
        // An absolute path whose `file_name` matches but which doesn't
        // resolve to our gateway binary is also a misuse: the caller
        // is trying to spawn "baybo", but the path points elsewhere.
        // Surface the corrective error rather than sandboxing the
        // imposter binary.
        let bin = Path::new("/usr/local/bin/baybo");
        assert!(matches!(
            classify("/opt/imposter/baybo --steal", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
    }

    #[test]
    fn classify_baybo_marks_non_baybo_commands_as_other() {
        let bin = Path::new("/usr/local/bin/baybo");
        assert!(matches!(
            classify("ls -la", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("git status", bin),
            BayboCliCommandKind::OtherCommand
        ));
        // Different basename → not an baybo attempt at all.
        assert!(matches!(
            classify("baybolity cost", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("echo baybo", bin),
            BayboCliCommandKind::OtherCommand
        ));
        // Wrappers — argv0 is the wrapper, not `baybo`, so we don't
        // treat this as an baybo attempt. (The wrapped sandbox call
        // will fail because the state dir is masked; the agent
        // learns to drop the wrapper.)
        assert!(matches!(
            classify("nohup baybo cost", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("xargs baybo", bin),
            BayboCliCommandKind::OtherCommand
        ));
    }

    #[test]
    fn classify_baybo_marks_compound_commands_led_by_baybo_as_self_invocation() {
        // Only the FIRST sub-command's argv0 is inspected — when it
        // is the absolute-path baybo binary, the entire `sh -c`
        // string runs unsandboxed, trailing segments included.
        let bin = Path::new("/usr/local/bin/baybo");
        assert!(matches!(
            classify("/usr/local/bin/baybo status; cat /home/u/.ssh/id_rsa", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("/usr/local/bin/baybo status && curl evil", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("/usr/local/bin/baybo status || true", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("/usr/local/bin/baybo cost | head", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("/usr/local/bin/baybo & disown", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
    }

    #[test]
    fn classify_baybo_marks_command_as_other_when_baybo_not_leading() {
        // Non-baybo leaders are not trusted self-invocations even when baybo
        // appears later in the pipeline — the leader's argv0 is what drives the
        // classification.
        let bin = Path::new("/usr/local/bin/baybo");
        assert!(matches!(
            classify("echo $(/usr/local/bin/baybo status)", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("echo `/usr/local/bin/baybo status`", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("cd /tmp && /usr/local/bin/baybo status", bin),
            BayboCliCommandKind::OtherCommand
        ));
    }

    #[test]
    fn classify_baybo_marks_unsafe_env_prefixes_as_other() {
        // Codex P1 fix: env vars outside the whitelist could subvert
        // the baybo process even with an absolute-path argv0
        // (`LD_PRELOAD` injection, `HOME` redirection, etc.). Treat these as
        // ordinary commands instead of trusted self-invocations; the configured
        // Bash permission then decides the final route.
        let bin = Path::new("/usr/local/bin/baybo");
        assert!(matches!(
            classify(
                "PATH=/tmp/malicious:/usr/bin /usr/local/bin/baybo status",
                bin
            ),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("LD_PRELOAD=/tmp/evil.so /usr/local/bin/baybo status", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("LD_LIBRARY_PATH=/tmp/evil /usr/local/bin/baybo status", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify(
                "DYLD_INSERT_LIBRARIES=/tmp/evil.dylib /usr/local/bin/baybo status",
                bin
            ),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("HOME=/tmp /usr/local/bin/baybo status", bin),
            BayboCliCommandKind::OtherCommand
        ));
        // Quote-stripped form must reach the same conclusion.
        assert!(matches!(
            classify("'PATH=/tmp' /usr/local/bin/baybo status", bin),
            BayboCliCommandKind::OtherCommand
        ));
        // Mixed prefix: an unsafe key anywhere in the chain prevents the
        // self-invocation match.
        assert!(matches!(
            classify("BAYBO_LOG=trace PATH=/tmp /usr/local/bin/baybo status", bin),
            BayboCliCommandKind::OtherCommand
        ));
    }

    #[test]
    fn classify_baybo_uses_bin_file_name() {
        // If the gateway was installed under a different file_name
        // (`baybo2`), `baybo` is no longer a baybo self-invocation attempt —
        // it's just an unrelated command. The new basename drives both the
        // `looks like baybo` check and the canonical-path self-invocation match.
        let bin = Path::new("/usr/local/bin/baybo2");
        assert!(matches!(
            classify("baybo cost", bin),
            BayboCliCommandKind::OtherCommand
        ));
        assert!(matches!(
            classify("baybo2 cost", bin),
            BayboCliCommandKind::NonCanonicalSelfInvocation
        ));
        assert!(matches!(
            classify("/usr/local/bin/baybo2 cost", bin),
            BayboCliCommandKind::CanonicalSelfInvocation
        ));
    }

    #[test]
    fn sandboxed_description_renders_without_leftover_placeholders() {
        let d = BashTool::new(
            baybo_workspace::WorkspacePaths::new("/some/ws"),
            baybo_process::ProcessManager::transient(),
        )
        .with_permission(BashPermissionMode::Manual)
        .description();
        assert!(
            !d.contains("{{"),
            "unfilled placeholder in description:\n{d}"
        );
        assert!(d.contains("SANDBOX:"), "isolation section present");
        assert!(d.contains("/some/ws/work"), "work dir filled");
        assert!(
            d.contains("SCRATCH:") && d.contains("/some/ws/work/tmp"),
            "sandboxed profile advertises the swept work/tmp scratch dir"
        );
    }

    #[tokio::test]
    async fn free_runs_without_a_backend_but_keeps_the_jail() {
        let work = std::path::Path::new("/tmp/work");
        let _ = std::fs::create_dir_all(work);
        // `free` Bash + a context with NO sandbox backend: it runs directly (no
        // OS sandbox), but the work-dir jail is still enforced at the tool layer.
        let tool = BashTool::new(
            baybo_workspace::WorkspacePaths::new("/tmp"),
            baybo_process::ProcessManager::transient(),
        )
        .with_permission(BashPermissionMode::Free);
        let ctx = ctx_with(None);

        // In-work command runs directly, no sandbox backend required.
        let marker = work.join(format!("free-marker-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let ok = serde_json::json!({
            "command": format!("touch {}", marker.display()),
            "cwd": "/tmp/work",
        });
        tool.execute(ok, &ctx)
            .await
            .expect("free runs an in-work command without a sandbox backend");
        assert!(marker.exists(), "free ran the command directly");
        let _ = std::fs::remove_file(&marker);

        // But a cwd OUTSIDE work/ is still rejected — unlike the bench profile,
        // `free` keeps the jail.
        let outside = serde_json::json!({ "command": "true", "cwd": "/tmp" });
        assert!(
            tool.execute(outside, &ctx).await.is_err(),
            "free keeps the work-dir jail (cwd outside work/ rejected)"
        );
    }

    #[test]
    fn free_description_drops_only_the_os_sandbox() {
        let free = BashTool::new(
            baybo_workspace::WorkspacePaths::new("/some/ws"),
            baybo_process::ProcessManager::transient(),
        )
        .with_permission(BashPermissionMode::Free);
        let d = free.description();
        assert!(
            !d.contains("{{"),
            "unfilled placeholder in free description"
        );
        // OS-sandbox claims dropped...
        assert!(
            !d.contains(SANDBOXED_MARKER),
            "free drops the OS-sandbox masking claim"
        );
        assert!(
            d.contains("OS sandbox is OFF"),
            "free says the sandbox is off"
        );
        // ...but the work-dir jail + uv shim are kept.
        assert!(
            d.contains("/some/ws/work"),
            "free keeps the work-dir scope section"
        );
        assert!(
            d.contains("uv run python"),
            "free keeps the uv-shimmed python"
        );
    }

    #[test]
    fn auto_description_advertises_the_risk_judge() {
        let auto = BashTool::new(
            baybo_workspace::WorkspacePaths::new("/some/ws"),
            baybo_process::ProcessManager::transient(),
        )
        .with_permission(auto_permission());
        let d = auto.description();
        assert!(
            !d.contains("{{"),
            "unfilled placeholder in auto description"
        );
        // Auto shares the sandbox surface with Sandboxed but advertises the
        // judge in its APPROVAL section.
        assert!(d.contains(SANDBOXED_MARKER), "auto is still sandboxed");
        assert!(
            d.contains("risk-judged") && d.contains("sandbox_escalation"),
            "auto description must describe the on-failure judge"
        );
    }

    #[test]
    fn permission_hot_swap_reskins_description_and_behavior() {
        let handle = Arc::new(LivePermissionMode::new(BashPermissionMode::Manual));
        let tool = BashTool::new(
            baybo_workspace::WorkspacePaths::new("/some/ws"),
            baybo_process::ProcessManager::transient(),
        )
        .with_permission_handle(Arc::clone(&handle));
        // Sandboxed: masked surface, OS sandbox on.
        assert!(tool.description().contains(SANDBOXED_MARKER));
        assert!(!tool.skip_os_sandbox());

        // Hot-swap to free via the shared handle: the SAME tool now skips the OS
        // sandbox but keeps uv + the work-dir scope — no rebuild.
        handle.set(BashPermissionMode::Free);
        assert!(tool.skip_os_sandbox());
        assert!(!tool.description().contains(SANDBOXED_MARKER));
        assert!(tool.description().contains("OS sandbox is OFF"));
        assert!(tool.description().contains("uv run python"));

        // And to auto: sandboxed surface + the judge note in APPROVAL.
        handle.set(auto_permission());
        assert!(!tool.skip_os_sandbox());
        assert!(tool.description().contains("risk-judged"));
    }

    #[test]
    fn free_keeps_uv_shims_in_wrap_command() {
        // `free` drops only the OS sandbox; python is still uv-shimmed (unlike
        // the bench profile). The uv exports + shim must survive.
        let free = BashTool::new(
            baybo_workspace::WorkspacePaths::new("/tmp"),
            baybo_process::ProcessManager::transient(),
        )
        .with_permission(BashPermissionMode::Free);
        let wrapped = free.wrap_command("python -c 'x'");
        assert!(
            wrapped.contains("uv run python"),
            "free must keep the uv shim: {wrapped}"
        );
        assert!(
            wrapped.contains("UV_CACHE_DIR"),
            "free must keep the uv exports: {wrapped}"
        );
    }

    /// The `bench-bash` profile (compile-time): run `cargo test -p baybo-tools
    /// --features bench-bash bench_profile` to exercise these. They assert the
    /// raw container behavior the feature switches on; the permission-specific tests
    /// above assume the feature is OFF (the default `cargo test`).
    #[cfg(feature = "bench-bash")]
    mod bench_profile {
        use super::*;

        #[test]
        fn description_is_the_raw_bench_prompt() {
            let cwd = std::env::current_dir().expect("cwd");
            let d = BashTool::new(
                baybo_workspace::WorkspacePaths::new("/some/ws"),
                baybo_process::ProcessManager::transient(),
            )
            .description();
            assert!(!d.contains("{{"), "unfilled placeholder");
            assert!(
                d.contains(&cwd.display().to_string()),
                "bench advertises the inherited cwd"
            );
            assert!(
                !d.contains("/some/ws/work"),
                "bench drops the work-dir jail"
            );
            assert!(!d.contains(SANDBOXED_MARKER));
            assert!(d.contains("own interpreters"), "bench uses native python");
            assert!(
                !d.contains("SCRATCH:"),
                "bench must not advertise a swept scratch dir (no janitor runs there)"
            );
        }

        #[test]
        fn wrap_command_skips_uv() {
            let w = BashTool::new(
                baybo_workspace::WorkspacePaths::new("/tmp"),
                baybo_process::ProcessManager::transient(),
            )
            .wrap_command("python -c 'x'");
            assert!(!w.contains("uv run"), "bench leaked uv shim: {w}");
            assert!(!w.contains("UV_CACHE_DIR"), "bench leaked uv exports: {w}");
        }
    }

    fn ctx_with(sandbox: Option<Arc<dyn crate::ExecSandbox>>) -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            sandbox,
            ..ToolContext::for_test()
        }
    }

    fn ctx_with_approval(
        sandbox: Option<Arc<dyn crate::ExecSandbox>>,
        gate: Arc<FakeApprovalGate>,
    ) -> ToolContext {
        let mut ctx = ctx_with(sandbox);
        let cache: Arc<Mutex<Vec<ApprovedResource>>> = Arc::new(Mutex::new(Vec::new()));
        ctx.approval = Some(ApprovalHandle::new(gate, cache, None));
        ctx
    }

    /// A `BackgroundJobSink` that records `(handle_id, command)` and drains
    /// the handed-off child so the test doesn't leak it.
    struct RecordingSink {
        seen: Arc<Mutex<Option<(String, String)>>>,
    }

    #[async_trait]
    impl crate::BackgroundJobSink for RecordingSink {
        async fn detach_command(&self, mut turn: crate::DetachedCommand) -> String {
            turn.child.start_kill();
            let _ = turn.child.wait().await;
            for t in turn.copy_tasks.drain(..) {
                let _ = t.await;
            }
            let handle = turn.handle_id.clone();
            *self.seen.lock() = Some((handle.clone(), turn.command.clone()));
            handle
        }
    }

    // ── auto permission ────────────────────────────────────────────────

    /// A `BilledChat` that replies with one canned verdict, for driving the
    /// risk judge deterministically.
    fn judge_llm(reply: &str) -> Arc<dyn baybo_llm::BilledChat> {
        use baybo_llm::test_support::StubLlm;
        use baybo_llm::{BillableLlm, LlmCompletion, LlmResponse, TokenUsage};
        let stub = Arc::new(StubLlm::new());
        stub.push_response(LlmResponse {
            content: reply.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: TokenUsage::default(),
            thinking: None,
        });
        crate::test_support::unbilled_chat(BillableLlm::passthrough(stub as Arc<dyn LlmCompletion>))
    }

    fn failed_out() -> crate::SandboxedOutput {
        crate::SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: b"boom".to_vec(),
            timed_out: false,
        }
    }

    const V_UNRELATED: &str =
        r#"{"sandbox_related":false,"risk":"safe","rationale":"compile error"}"#;
    const V_SAFE: &str = r#"{"sandbox_related":true,"risk":"safe","rationale":"needs ~/.aws"}"#;
    const V_RISKY: &str =
        r#"{"sandbox_related":true,"risk":"risky","rationale":"would delete creds"}"#;

    #[tokio::test]
    async fn escalate_noop_on_success() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let mut ctx = ctx_with(None);
        ctx.lite_llm = Some(judge_llm(V_SAFE)); // present but must not be consulted
        let ok = crate::SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        };
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                ok,
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(note.is_none());
    }

    #[tokio::test]
    async fn escalate_noop_when_policy_disallows_auto_escape() {
        let tool = BashTool::for_test();
        let ctx = ctx_with(None);
        let (out, note) = tool
            .escalate_if_failed(
                "x",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::None,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 1);
        assert!(note.is_none());
    }

    #[tokio::test]
    async fn escalate_without_judge_llm_prompts_when_possible() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(None, gate);
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(note.unwrap().contains("user approval"));
    }

    /// An ordinary failure (compile error, network flake, deliberate exit 1)
    /// is not the sandbox's doing, so it must surface as-is. Offering the
    /// full-host escape here would put the most privileged prompt in the system
    /// in front of the user on every non-zero exit.
    #[tokio::test]
    async fn escalate_keeps_failure_when_judge_says_not_sandbox_related() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let mut ctx = ctx_with_approval(None, gate.clone());
        ctx.lite_llm = Some(judge_llm(V_UNRELATED));
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 1, "original failure kept, no escape");
        assert!(note.is_none());
        assert!(
            gate.requests().is_empty(),
            "user must not be asked to approve an escape the judge never proposed"
        );
    }

    #[tokio::test]
    async fn escalate_runs_unsandboxed_when_safe() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let mut ctx = ctx_with(None);
        ctx.lite_llm = Some(judge_llm(V_SAFE));
        // `true` exits 0 unsandboxed, so a flip from the seeded exit 1 proves
        // the re-run happened.
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0, "command was re-run outside the sandbox");
        assert!(note.unwrap().contains("outside the OS sandbox"));
    }

    #[tokio::test]
    async fn escalate_risky_runs_after_approval() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let mut ctx = ctx_with_approval(None, gate);
        ctx.lite_llm = Some(judge_llm(V_RISKY));
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0, "approved → re-run unsandboxed");
        assert!(note.unwrap().contains("user approval"));
    }

    #[tokio::test]
    async fn escalate_risky_kept_when_denied() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Deny));
        let mut ctx = ctx_with_approval(None, gate);
        ctx.lite_llm = Some(judge_llm(V_RISKY));
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 1, "denied → original failure kept");
        assert!(note.is_none());
    }

    #[tokio::test]
    async fn escalate_risky_kept_when_unattended() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let mut ctx = ctx_with(None); // no approval handle (cron / subagent)
        ctx.lite_llm = Some(judge_llm(V_RISKY));
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(
            out.exit_code, 1,
            "no human → original failure kept, no escape"
        );
        assert!(note.is_none());
    }

    #[tokio::test]
    async fn escalate_honors_the_judge_snapshot_not_live_permission() {
        // Regression for the mid-command reload race: the tool's LIVE permission is
        // `free` (would never judge), but the per-command snapshot captured
        // AutoJudge (permission was `auto` at execute() entry). escalate must act
        // on the snapshot it was handed, not re-read the now-swapped permission.
        let tool = BashTool::for_test().with_permission(BashPermissionMode::Free);
        let mut ctx = ctx_with(None);
        ctx.lite_llm = Some(judge_llm(V_SAFE));
        let (out, note) = tool
            .escalate_if_failed(
                "true",
                None,
                failed_out(),
                &ChildEnv::default(),
                Duration::from_secs(5),
                &ctx,
                SandboxEscapePolicy::AutoJudge,
            )
            .await
            .unwrap();
        assert_eq!(
            out.exit_code, 0,
            "escalated per the snapshot, not live permission"
        );
        assert!(note.is_some());
    }

    #[tokio::test]
    async fn pre_exec_gate_proceeds_when_safe() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let mut ctx = ctx_with(None);
        ctx.lite_llm = Some(judge_llm(r#"{"risk":"safe","rationale":"scratch dir"}"#));
        tool.pre_exec_gate("rm -rf /tmp/scratch", None, &ctx, true)
            .await
            .expect("safe destructive command proceeds without approval");
    }

    /// A `ToolEventSink` that records every emitted `(action, payload)`.
    #[derive(Default)]
    struct RecordingEventSink {
        entries: Mutex<Vec<(String, ToolEventPayload)>>,
    }

    impl crate::ToolEventSink for RecordingEventSink {
        fn emit(&self, action: &str, payload: ToolEventPayload) {
            self.entries.lock().push((action.to_string(), payload));
        }
    }

    #[tokio::test]
    async fn risk_judge_records_llm_call_input_output_and_duration() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let mut ctx = ctx_with(None);
        ctx.lite_llm = Some(judge_llm(r#"{"risk":"safe","rationale":"scratch dir"}"#));
        let events = Arc::new(RecordingEventSink::default());
        ctx.events = Arc::clone(&events) as Arc<dyn crate::ToolEventSink>;

        tool.pre_exec_gate("rm -rf /tmp/scratch", None, &ctx, true)
            .await
            .expect("safe verdict proceeds");

        let recorded = events.entries.lock();
        assert!(
            recorded.iter().any(|(a, p)| a == "risk_judge"
                && matches!(
                    p,
                    ToolEventPayload::LlmCall { input, output, .. }
                        if input.contains("rm -rf /tmp/scratch") && output.contains("safe")
                )),
            "expected an llm_call event carrying judge input+output, got {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|(a, p)| a == "risk_judge" && matches!(p, ToolEventPayload::Phase { .. })),
            "expected a phase (duration) event for the judge call, got {recorded:?}"
        );
    }

    /// `extra_env` empty is the common case, and the one where a gated
    /// sanitizer would ship the raw capture to the provider.
    #[tokio::test]
    async fn judge_input_is_sanitized_even_without_injected_secret_env() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let events = Arc::new(RecordingEventSink::default());
        let mut ctx = ctx_with(None);
        ctx.lite_llm = Some(judge_llm(V_UNRELATED));
        ctx.secrets = Some(Arc::new(StubSecrets) as Arc<dyn crate::SecretAccess>);
        ctx.events = Arc::clone(&events) as Arc<dyn crate::ToolEventSink>;

        let leaky = crate::SandboxedOutput {
            exit_code: 1,
            stdout: format!("token={STUB_DETECTED_SECRET}\n").into_bytes(),
            stderr: Vec::new(),
            timed_out: false,
        };

        tool.escalate_if_failed(
            "curl https://example.com",
            None,
            leaky,
            &ChildEnv::default(), // no injected secrets — the path that skipped redaction
            Duration::from_secs(5),
            &ctx,
            SandboxEscapePolicy::AutoJudge,
        )
        .await
        .expect("escalation decision");

        let recorded = events.entries.lock();
        let judge_input = recorded
            .iter()
            .find_map(|(a, p)| match p {
                ToolEventPayload::LlmCall { input, .. } if a == "unsandbox_judge" => Some(input),
                _ => None,
            })
            .expect("judge llm_call recorded");
        assert!(
            !judge_input.contains(STUB_DETECTED_SECRET),
            "raw secret reached the judge prompt: {judge_input}"
        );
        assert!(
            judge_input.contains("[SANITIZED]"),
            "detector pass did not run: {judge_input}"
        );
    }

    #[tokio::test]
    async fn unparsable_command_records_parse_failure_event() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: b"ok\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let events = Arc::new(RecordingEventSink::default());
        let mut ctx = ctx_with(Some(sandbox));
        ctx.events = Arc::clone(&events) as Arc<dyn crate::ToolEventSink>;

        // Unterminated quote → brush can't parse it → the detector falls back
        // to the keyword pre-filter, and the parse gap is recorded.
        let _ = BashTool::for_test()
            .execute(json!({ "command": "echo it's fine" }), &ctx)
            .await;

        let recorded = events.entries.lock();
        assert!(
            recorded.iter().any(|(a, p)| a == "delete_scan"
                && matches!(
                    p,
                    ToolEventPayload::ParseFailure { command } if command.contains("echo it's")
                )),
            "expected a parse_failure event, got {recorded:?}"
        );
    }

    #[tokio::test]
    async fn pre_exec_gate_denied_errors() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Deny));
        let mut ctx = ctx_with_approval(None, gate);
        ctx.lite_llm = Some(judge_llm(r#"{"risk":"risky","rationale":"rm of source"}"#));
        let err = tool
            .pre_exec_gate("rm -rf src", None, &ctx, true)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn pre_exec_gate_errors_when_unattended_and_risky() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let mut ctx = ctx_with(None); // no approval handle
        ctx.lite_llm = Some(judge_llm(r#"{"risk":"risky","rationale":"rm of source"}"#));
        let err = tool
            .pre_exec_gate("rm -rf src", None, &ctx, true)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn pre_exec_gate_without_llm_requires_approval() {
        let tool = BashTool::for_test().with_permission(auto_permission());
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(None, gate); // ctx.lite_llm = None → fail-closed prompt
        tool.pre_exec_gate("rm -rf x", None, &ctx, true)
            .await
            .expect("no judge → prompt, approval granted → proceed");
    }

    #[tokio::test]
    async fn default_auto_judges_destructive_commands_inside_sandbox() {
        let _ = std::fs::create_dir_all("/tmp/work");
        let tool = BashTool::new(
            baybo_workspace::WorkspacePaths::new("/tmp"),
            baybo_process::ProcessManager::transient(),
        );
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Deny));
        let sandbox: Arc<dyn crate::ExecSandbox> = Arc::new(FakeExecSandbox::new());
        let mut ctx = ctx_with_approval(Some(sandbox), Arc::clone(&gate));
        ctx.lite_llm = Some(judge_llm(
            r#"{"risk":"risky","rationale":"would delete source"}"#,
        ));

        let err = tool
            .execute(
                json!({
                    "command": "rm -rf /tmp/work/default-auto-risky",
                    "cwd": "/tmp/work",
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Execution(_)));
        let requests = gate.requests();
        assert_eq!(requests.len(), 1, "expected the auto permission prompt");
        assert!(
            requests[0].params_preview.contains("inside the OS sandbox"),
            "default auto prompt must describe the sandboxed route: {}",
            requests[0].params_preview,
        );
    }

    /// A ctx whose `workspace_paths` point at a unique temp dir (so
    /// `logs_dir()/background` is writable) with a recording sink wired in.
    #[allow(clippy::type_complexity)]
    fn ctx_for_detached() -> (ToolContext, Arc<Mutex<Option<(String, String)>>>) {
        let tmp = std::env::temp_dir().join(format!("baybo-bgtest-{}", uuid::Uuid::new_v4()));
        // `run_detached` detaches through `ctx.sandbox`; the fake spawns the
        // command directly so the test needs no real OS sandbox.
        let mut ctx = ctx_with(Some(Arc::new(FakeExecSandbox::new())));
        ctx.workspace_paths = baybo_workspace::WorkspacePaths::new(&tmp);
        let seen = Arc::new(Mutex::new(None));
        ctx.background_jobs = Some(Arc::new(RecordingSink {
            seen: Arc::clone(&seen),
        }));
        (ctx, seen)
    }

    #[tokio::test]
    async fn detached_command_completing_in_window_returns_normal_result() {
        let (ctx, seen) = ctx_for_detached();
        let sink = ctx.background_jobs.clone().unwrap();
        let args = vec!["-c".into(), "echo hi".into()];
        let tool = BashTool::for_test();
        let out = run_detached(
            &tool,
            DetachedExecutionRoute::Sandboxed,
            "echo hi",
            &args,
            None,
            &ChildEnv::default(),
            Duration::from_secs(5),
            &ctx,
            &sink,
            SandboxEscapePolicy::None,
        )
        .await
        .expect("run_detached ok");
        let Some(ToolOutput::Json(v)) = out else {
            panic!("expected a completed Json result, got {out:?}");
        };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("hi"));
        assert!(
            seen.lock().is_none(),
            "a fast command must NOT be handed to the background sink"
        );
    }

    #[tokio::test]
    async fn detached_command_overrunning_budget_goes_to_background() {
        let (ctx, seen) = ctx_for_detached();
        let sink = ctx.background_jobs.clone().unwrap();
        let args = vec!["-c".into(), "sleep 30".into()];
        let tool = BashTool::for_test();
        let out = run_detached(
            &tool,
            DetachedExecutionRoute::Sandboxed,
            "sleep 30",
            &args,
            None,
            &ChildEnv::default(),
            Duration::from_millis(150),
            &ctx,
            &sink,
            SandboxEscapePolicy::None,
        )
        .await
        .expect("run_detached ok");
        let Some(ToolOutput::Text(text)) = out else {
            panic!("expected a background notice, got {out:?}");
        };
        assert!(text.contains("background"), "notice: {text}");
        let recorded = seen.lock().clone();
        assert_eq!(
            recorded.map(|(_, cmd)| cmd),
            Some("sleep 30".to_string()),
            "an overrunning command must be handed to the sink"
        );
    }

    #[tokio::test]
    async fn execute_with_free_permission_can_background_unsandboxed_command() {
        let (mut ctx, seen) = ctx_for_detached();
        ctx.sandbox = None;
        let tool = BashTool::for_test().with_permission(BashPermissionMode::Free);

        let out = tool
            .execute(
                json!({
                    "command": "sleep 30",
                    "timeout_ms": 150,
                }),
                &ctx,
            )
            .await
            .expect("unsandboxed command backgrounds");

        let ToolOutput::Text(text) = out else {
            panic!("expected a background notice, got {out:?}");
        };
        assert!(text.contains("background"), "notice: {text}");
        let recorded = seen.lock().clone();
        assert_eq!(
            recorded.map(|(_, cmd)| cmd),
            Some("sleep 30".to_string()),
            "permission=free must use the detached background path"
        );
    }

    /// An ineligible turn (a cron fire's own run, a nested subagent) keeps
    /// kill-on-timeout even with a sink wired: the sink is a capability, the
    /// gate is `background_eligible`. Same command + timeout as the test
    /// above, which backgrounds.
    #[tokio::test]
    async fn an_ineligible_turn_kills_on_timeout_despite_a_wired_sink() {
        let (mut ctx, seen) = ctx_for_detached();
        ctx.sandbox = None;
        ctx.background_eligible = false;
        let tool = BashTool::for_test().with_permission(BashPermissionMode::Free);

        let err = tool
            .execute(
                json!({
                    "command": "sleep 30",
                    "timeout_ms": 150,
                }),
                &ctx,
            )
            .await
            .expect_err("an ineligible turn must time out, not background");

        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected a timeout error, got {err:?}"
        );
        assert!(
            seen.lock().is_none(),
            "an ineligible turn must not reach the background sink"
        );
    }

    #[tokio::test]
    async fn secret_env_can_background_and_warns_about_raw_output() {
        let (mut ctx, seen) = ctx_for_detached();
        ctx.sandbox = None;
        ctx.secrets = Some(Arc::new(StubSecrets));
        let tool = BashTool::for_test().with_permission(BashPermissionMode::Free);

        let out = tool
            .execute(
                json!({
                    "command": "sleep 30",
                    "timeout_ms": 150,
                    "secret_env": ["TESTTOKEN"],
                }),
                &ctx,
            )
            .await
            .expect("secret_env command backgrounds");

        let ToolOutput::Text(text) = out else {
            panic!("expected a background notice, got {out:?}");
        };
        assert!(text.contains("background"), "notice: {text}");
        assert!(
            text.contains("not secret-redacted"),
            "secret_env background notice must record raw-output risk: {text}"
        );
        let recorded = seen.lock().clone();
        assert_eq!(
            recorded.map(|(_, cmd)| cmd),
            Some("sleep 30".to_string()),
            "secret_env must not prevent background handoff"
        );
    }

    #[tokio::test]
    async fn detached_unsandboxed_cancel_reaps_process_group() {
        let (ctx, _seen) = ctx_for_detached();
        let sink = ctx.background_jobs.clone().unwrap();
        let pidfile = std::env::temp_dir().join(format!(
            "baybo-bg-pgkill-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&pidfile);

        let script = format!(
            "sleep 30 & echo $! > {}; wait",
            sh_quote(&pidfile.to_string_lossy())
        );
        let args = vec!["-c".into(), script.clone()];
        let tool = BashTool::for_test();
        let out = run_detached(
            &tool,
            DetachedExecutionRoute::Unsandboxed,
            &script,
            &args,
            None,
            &ChildEnv::default(),
            Duration::from_millis(150),
            &ctx,
            &sink,
            SandboxEscapePolicy::None,
        )
        .await
        .expect("run_detached ok");
        let Some(ToolOutput::Text(text)) = out else {
            panic!("expected a background notice, got {out:?}");
        };
        assert!(text.contains("background"), "notice: {text}");

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pidfile written")
            .trim()
            .parse()
            .expect("pid parses");
        let _ = std::fs::remove_file(&pidfile);
        let reaped = wait_until_pid_gone(pid).await;
        if !reaped {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(
            reaped,
            "grandchild pid {pid} survived detached cancellation"
        );
    }

    #[test]
    fn prune_outputs_respects_cutoff() {
        use std::time::{Duration as Dur, SystemTime};
        let dir = std::env::temp_dir().join(format!("baybo-prune-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("turn.out");
        std::fs::write(&f, b"x").unwrap();

        // Cutoff in the past → the just-written file is newer → kept.
        assert_eq!(
            prune_outputs_before(&dir, SystemTime::now() - Dur::from_secs(3600)),
            0
        );
        assert!(f.exists(), "a fresh file must not be pruned");

        // Cutoff in the future → the file is older than it → pruned.
        assert_eq!(
            prune_outputs_before(&dir, SystemTime::now() + Dur::from_secs(3600)),
            1
        );
        assert!(!f.exists(), "an aged-out file must be pruned");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_issue_runs_commands_in_its_checkout_by_default() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let mut ctx = ctx_with(Some(sandbox));
        let checkout = PathBuf::from("/tmp/work/projects/p/4");
        ctx.checkout_root = Some(checkout.clone());

        BashTool::for_test()
            .execute(json!({ "command": "git status" }), &ctx)
            .await
            .expect("bash runs");

        assert_eq!(fake.calls()[0].cwd.as_ref(), Some(&checkout));
    }

    #[tokio::test]
    async fn an_explicit_cwd_still_beats_the_checkout() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let mut ctx = ctx_with(Some(sandbox));
        ctx.checkout_root = Some(PathBuf::from("/tmp/work/projects/p/4"));

        BashTool::for_test()
            .execute(
                json!({ "command": "ls", "cwd": "/tmp/work/elsewhere" }),
                &ctx,
            )
            .await
            .expect("bash runs");

        assert_eq!(
            fake.calls()[0].cwd.as_deref(),
            Some(Path::new("/tmp/work/elsewhere")),
            "the checkout is a default, not an override"
        );
    }

    #[tokio::test]
    async fn a_session_with_no_checkout_still_defaults_to_the_work_dir() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let ctx = ctx_with(Some(sandbox));

        BashTool::for_test()
            .execute(json!({ "command": "ls" }), &ctx)
            .await
            .expect("bash runs");

        assert_eq!(
            fake.calls()[0].cwd.as_deref(),
            Some(ctx.workspace_root.as_path()),
            "an ordinary session must be unchanged by the checkout default"
        );
    }

    #[tokio::test]
    async fn an_issue_run_commits_as_the_agent_working_the_card() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let mut ctx = ctx_with(Some(sandbox));
        let id = baybo_model::AgentProfileId::generate();
        ctx.checkout_root = Some(PathBuf::from("/tmp/work/projects/p/4"));
        ctx.agent_id = id.clone();
        ctx.agent_handle = Some(baybo_model::AgentHandle::parse("dev-1").expect("handle"));

        BashTool::for_test()
            .execute(json!({ "command": "git commit -m x" }), &ctx)
            .await
            .expect("bash runs");

        let env = &fake.calls()[0].extra_env;
        let value = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("{key} must reach the child: {env:?}"))
        };
        assert_eq!(value("GIT_AUTHOR_NAME"), "dev-1");
        assert_eq!(value("GIT_COMMITTER_NAME"), "dev-1");
        let email = format!("{id}@baybo.local");
        assert_eq!(value("GIT_AUTHOR_EMAIL"), email);
        assert_eq!(value("GIT_COMMITTER_EMAIL"), email);
        assert!(
            !env.iter()
                .any(|(k, v)| k.ends_with("_NAME") && v.contains(id.as_str())),
            "the ULID must not be what `git log` shows as the author: {env:?}"
        );
    }

    #[tokio::test]
    async fn an_issue_run_whose_agent_has_no_handle_falls_back_to_the_workspace_identity() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let mut ctx = ctx_with(Some(sandbox));
        ctx.checkout_root = Some(PathBuf::from("/tmp/work/projects/p/4"));
        ctx.agent_id = baybo_model::AgentProfileId::generate();

        BashTool::for_test()
            .execute(json!({ "command": "git commit -m x" }), &ctx)
            .await
            .expect("bash runs");

        assert!(
            !fake.calls()[0]
                .extra_env
                .iter()
                .any(|(k, _)| k.starts_with("GIT_")),
            "a commit git authors from the workspace `.gitconfig` says baybo, which is \
             true; one authored by a ULID says nothing"
        );
    }

    #[tokio::test]
    async fn an_ordinary_session_carries_no_git_identity() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let ctx = ctx_with(Some(sandbox));

        BashTool::for_test()
            .execute(json!({ "command": "ls" }), &ctx)
            .await
            .expect("bash runs");

        assert!(
            !fake.calls()[0]
                .extra_env
                .iter()
                .any(|(k, _)| k.starts_with("GIT_")),
            "a session that never commits gains nothing from an identity"
        );
    }

    #[tokio::test]
    async fn the_agents_own_id_is_not_scrubbed_out_of_its_output() {
        let id = baybo_model::AgentProfileId::generate();
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: format!("switched to branch issue/4-x by dev-1 ({id})\n").into_bytes(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let mut ctx = ctx_with(Some(sandbox));
        ctx.secrets = Some(Arc::new(StubSecrets));
        ctx.checkout_root = Some(PathBuf::from("/tmp/work/projects/p/4"));
        ctx.agent_id = id.clone();
        ctx.agent_handle = Some(baybo_model::AgentHandle::parse("dev-1").expect("handle"));

        let out = BashTool::for_test()
            .execute(json!({ "command": "git status" }), &ctx)
            .await
            .expect("bash runs");

        let ToolOutput::Json(v) = out else {
            panic!("bash returns json");
        };
        let stdout = v["stdout"].as_str().expect("stdout");
        assert!(
            stdout.contains("dev-1") && stdout.contains(id.as_str()),
            "the agent's own handle and id must survive its own output: {stdout}"
        );
    }

    fn fake_with_response(
        out: SandboxedOutput,
    ) -> (Arc<FakeExecSandbox>, Arc<dyn crate::ExecSandbox>) {
        let fake = Arc::new(FakeExecSandbox::new());
        fake.set_response(out);
        let dyn_handle: Arc<dyn crate::ExecSandbox> = fake.clone();
        (fake, dyn_handle)
    }

    /// Minimal `SecretAccess` for bash tests: resolves each name to
    /// `VAL_<name>`, redacts those known values to `[REDACTED]`, and stands in
    /// for the leak detector by rewriting a single sentinel token.
    struct StubSecrets;

    /// What [`StubSecrets::sanitize`] treats as a detector hit, so tests can
    /// prove the detector pass runs without depending on real rule patterns.
    const STUB_DETECTED_SECRET: &str = "SENTINEL_LEAKED_SECRET";

    #[async_trait::async_trait]
    impl crate::SecretAccess for StubSecrets {
        async fn resolve_env(&self, names: &[String]) -> crate::Result<Vec<(String, String)>> {
            Ok(names
                .iter()
                .map(|n| (n.clone(), format!("VAL_{n}")))
                .collect())
        }
        async fn redact(&self, text: &str, values: &[String]) -> crate::Result<String> {
            let mut out = text.to_string();
            for v in values {
                out = out.replace(v, "[REDACTED]");
            }
            Ok(out)
        }
        async fn sanitize(&self, text: &str) -> crate::Result<String> {
            Ok(text.replace(STUB_DETECTED_SECRET, "[SANITIZED]"))
        }
        async fn add(
            &self,
            _name: &str,
            _value: &[u8],
            _overwrite: bool,
        ) -> crate::Result<baybo_security::AddOutcome> {
            unreachable!("bash never adds secrets")
        }
        async fn list_names(&self) -> crate::Result<Vec<String>> {
            unreachable!("bash never lists secrets")
        }
        async fn exists(&self, _names: &[String]) -> crate::Result<Vec<(String, bool)>> {
            unreachable!("bash never checks secrets")
        }
    }

    #[tokio::test]
    async fn secret_env_injects_into_child_and_redacts_output() {
        // The fake sandbox echoes the resolved secret value back in stdout so we
        // can assert it is scrubbed before the tool result returns.
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: b"token=VAL_TESTTOKEN done\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let mut ctx = ctx_with(Some(sandbox));
        ctx.secrets = Some(Arc::new(StubSecrets));

        let out = BashTool::for_test()
            .execute(
                json!({ "command": "echo hi", "secret_env": ["TESTTOKEN"] }),
                &ctx,
            )
            .await
            .expect("bash with secret_env runs");

        // (1) the resolved value reached the child via SpawnOpts.extra_env.
        let call = &fake.calls()[0];
        assert!(
            call.extra_env
                .iter()
                .any(|(k, v)| k == "TESTTOKEN" && v == "VAL_TESTTOKEN"),
            "secret must be injected into the child env: {:?}",
            call.extra_env
        );
        // (2) the value is scrubbed out of the returned output.
        let ToolOutput::Json(v) = out else {
            panic!("expected json")
        };
        let stdout = v["stdout"].as_str().expect("stdout string");
        assert!(
            !stdout.contains("VAL_TESTTOKEN"),
            "secret value must be redacted from stdout: {stdout}"
        );
        assert!(stdout.contains("[REDACTED]"), "stdout: {stdout}");
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
            _opts: crate::SpawnOpts,
        ) -> crate::Result<SandboxedOutput> {
            Err(ToolError::Execution(self.message.clone()))
        }
    }

    struct RecordingNotifier {
        notices: Arc<Mutex<Vec<(crate::NoticeLevel, String, String)>>>,
    }

    impl crate::SessionNotifier for RecordingNotifier {
        fn emit(&self, level: crate::NoticeLevel, summary: &str, detail: &str) {
            self.notices
                .lock()
                .push((level, summary.to_string(), detail.to_string()));
        }
    }

    #[tokio::test]
    async fn missing_sandbox_backend_notices_and_runs_unsandboxed() {
        let notices = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = ctx_with(None);
        ctx.sandbox_bypass_reason = Some("sandbox backend unavailable in test".to_string());
        ctx.notifier = Some(Arc::new(RecordingNotifier {
            notices: Arc::clone(&notices),
        }));

        let out = BashTool::for_test()
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .expect("missing sandbox backend should downgrade to unsandboxed");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap_or("").contains("hi"));

        let notices = notices.lock().clone();
        assert_eq!(notices.len(), 1, "expected one sandbox bypass notice");
        assert_eq!(notices[0].0, crate::NoticeLevel::Warn);
        assert!(notices[0].1.contains("without the OS sandbox"));
        assert!(notices[0].2.contains("sandbox backend unavailable in test"));
        assert!(notices[0].2.contains("echo hi"));
    }

    #[tokio::test]
    async fn outer_container_sandbox_bypass_runs_without_notice() {
        let notices = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = ctx_with(None);
        ctx.notifier = Some(Arc::new(RecordingNotifier {
            notices: Arc::clone(&notices),
        }));

        let out = BashTool::for_test()
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .expect("container sandbox bypass should still run unsandboxed");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap_or("").contains("hi"));
        assert!(
            notices.lock().is_empty(),
            "outer-container sandbox bypass should not notify the user"
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
        let tool = BashTool::for_test();
        let out = tool
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
        // Tighter than `contains` — locks the whole uv env prefix (incl.
        // the optional leading PATH export) to the head so a future
        // reorder can't accidentally drop it and still pass.
        assert!(
            calls[0].args[1].starts_with(tool.uv_env_prefix.as_str()),
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
    async fn execute_rejects_bare_baybo_with_absolute_path_error() {
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
    async fn canonical_baybo_invocations_run_unsandboxed() {
        // `baybo …` commands must NOT consult the sandbox: the sandbox
        // masks `~/.baybo`/`$BAYBO_HOME`, so a sandboxed baybo process
        // can't see the gateway's config or session store. Two
        // assertions:
        //   1. The fake sandbox is never invoked.
        //   2. Even without ANY sandbox configured, the command still
        //      runs (no "OS sandbox unavailable" error).
        //
        // The match is keyed off the running binary's `current_exe()`
        // — under `cargo test` that's the test harness binary (e.g.
        // `baybo_tools-XXXX`), NOT `baybo`. So we drive the self-invocation
        // route with the absolute test-binary path; the underlying `sh -c` will
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
        let cmd = format!("{exe_path} --baybo-self-probe-nonexistent-arg");
        let out = BashTool::for_test()
            .execute(
                json!({ "command": cmd, "timeout_ms": 5000 }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .expect("baybo command must run unsandboxed");
        let ToolOutput::Json(_) = out else { panic!() };
        assert!(
            fake.calls().is_empty(),
            "baybo invocations must skip the sandbox: {:?}",
            fake.calls()
        );

        // And the self-invocation route works even when no sandbox is installed.
        let cmd = format!("{exe_path} --baybo-self-probe-nonexistent-arg");
        BashTool::for_test()
            .execute(
                json!({ "command": cmd, "timeout_ms": 5000 }),
                &ctx_with(None),
            )
            .await
            .expect("baybo command must run even without a sandbox");
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
        let tool = BashTool::for_test();
        tool.execute(json!({ "command": "pwd" }), &ctx_with(Some(sandbox)))
            .await
            .expect("pwd must run");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "pwd must consult the sandbox now");
        assert_eq!(calls[0].args[0], "-c");
        assert!(
            calls[0].args[1].starts_with(tool.uv_env_prefix.as_str()),
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

    /// Running an installed skill's bundled script in place
    /// (`python <its own skills dir>/.../x.py`) must pass the work-dir guard
    /// even though the skill tree is outside `work/` — the sandbox binds it
    /// read-only. Regression test for the rejection that forced skills to be
    /// copied into `work/` first. The path comes from the ctx's own agent
    /// rather than a literal, so relocating the tree cannot quietly turn this
    /// into a test of some other directory.
    #[tokio::test]
    async fn accepts_command_argument_under_the_callers_own_skill_dir() {
        for agent in ["baybo", "01JCUSTOM"] {
            let (_fake, sandbox) = fake_with_response(SandboxedOutput {
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: false,
            });
            let agent = baybo_model::AgentProfileId::parse(agent).expect("valid id");
            let script = WorkspacePaths::new("/tmp")
                .persona_skills_dir(agent.as_str())
                .join("mmemos-memo/scripts/mmemos.py");
            let mut ctx = ctx_with(Some(sandbox));
            ctx.agent_id = agent;
            ctx.workspace_paths = WorkspacePaths::new("/tmp");
            BashTool::for_test()
                .execute(
                    json!({
                        "command": format!("python {} auth-status", script.display()),
                        "cwd": "/tmp/work"
                    }),
                    &ctx,
                )
                .await
                .expect("the caller's own skill dir must be accepted");
        }
    }

    /// …but only its *own*. Another agent's directory is not bound into this
    /// session's sandbox, so naming a path there is the ordinary
    /// outside-`work/` rejection rather than a silently broken exec.
    #[tokio::test]
    async fn rejects_command_argument_under_another_agents_skill_dir() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let script = WorkspacePaths::new("/tmp")
            .persona_skills_dir("01JSOMEONEELSE")
            .join("theirs/scripts/run.py");
        let mut ctx = ctx_with(Some(sandbox));
        ctx.agent_id = baybo_model::AgentProfileId::parse("01JCUSTOM").expect("valid id");
        ctx.workspace_paths = WorkspacePaths::new("/tmp");
        let err = BashTool::for_test()
            .execute(
                json!({
                    "command": format!("python {} --check", script.display()),
                    "cwd": "/tmp/work"
                }),
                &ctx,
            )
            .await
            .expect_err("another agent's skill dir is not exempt");
        assert!(matches!(err, ToolError::InvalidParams(_)), "got {err:?}");
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
        let skills = WorkspacePaths::new("/tmp").persona_skills_dir("01JCUSTOM");

        // Clean command — no offending paths.
        assert!(
            require_command_paths_within_work_dir(
                "ls /tmp/work && cat /etc/hosts",
                ws,
                work,
                &skills
            )
            .is_ok()
        );

        // Quoted path inside the workspace but outside work — caught
        // after `shell_words::split` unquotes the token.
        let err =
            require_command_paths_within_work_dir(r#"ls "/tmp/profile/foo""#, ws, work, &skills)
                .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(ref m) if m.contains("/tmp/profile/foo")));

        // Path hidden behind a pipeline still gets walked.
        let err = require_command_paths_within_work_dir(
            "git status | tee /tmp/logs/out",
            ws,
            work,
            &skills,
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(ref m) if m.contains("/tmp/logs/out")));

        // A path under the exempt skill dir is allowed — running an
        // installed skill's bundled script in place is the intended use, and
        // the RO sandbox bind makes the path safe to name.
        let script = skills.join("mmemos-memo/scripts/mmemos.py");
        assert!(
            require_command_paths_within_work_dir(
                &format!("python {} auth-status", script.display()),
                ws,
                work,
                &skills
            )
            .is_ok()
        );
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
            msg.contains("bwrap setup failure") && msg.contains("not approved"),
            "deny should annotate that the unsandboxed retry was not approved: {msg}"
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
            msg.contains("not approved") && msg.contains("bwrap setup failure"),
            "error must explain that retry was not approved AND keep original reason: {msg}"
        );
    }

    #[tokio::test]
    async fn manual_nonzero_exit_prompts_for_unsandboxed_retry() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: b"command failed\n".to_vec(),
            timed_out: false,
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        // Manual permission asks again before a sandbox-failure escape. A
        // non-zero command result is treated as failed sandboxed execution.
        let out = BashTool::for_test()
            .execute(json!({ "command": "false" }), &ctx)
            .await
            .expect("non-zero exit returns Ok");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 1);
        assert_eq!(
            gate.requests().len(),
            1,
            "manual failure should prompt once for unsandboxed retry"
        );
    }

    /// End-to-end shape of the reported regression: a curl that fails on the
    /// network is not a sandbox problem, so it must come back as a plain
    /// failure the model can read and retry — not as an escape prompt.
    #[tokio::test]
    async fn auto_network_failure_keeps_result_without_prompting() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 6,
            stdout: Vec::new(),
            stderr: b"curl: (6) Could not resolve host: example.com\n".to_vec(),
            timed_out: false,
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Deny));
        let mut ctx = ctx_with_approval(Some(sandbox), gate.clone());
        ctx.lite_llm = Some(judge_llm(V_UNRELATED));

        let out = BashTool::for_test()
            .with_permission(auto_permission())
            .execute(json!({ "command": "curl https://example.com" }), &ctx)
            .await
            .expect("an ordinary failure returns Ok");
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
            "a non-sandbox failure must not raise an escape prompt"
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
    fn manual_accessed_resources_prompts_for_every_executable_command() {
        // FileToolRedirect rejection bypasses approval — the sandbox
        // never gets reached.
        let resources = BashTool::for_test().accessed_resources(&json!({ "command": "cat foo" }));
        assert!(
            resources.is_empty(),
            "content-read is rejected before sandbox; no approval needed"
        );

        // Manual permission: every executable Bash command declares
        // ExecCommand for the executor's human approval gate.
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
            assert_eq!(
                resources.len(),
                1,
                "{cmd:?} should declare ExecCommand for approval, got {resources:?}"
            );
        }

        // Destructive commands still declare the same single ExecCommand
        // resource; the destructive label is separate UI metadata.
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
    fn accessed_resources_only_manual_permission_prompts() {
        let destructive = json!({ "command": "rm -rf /tmp/work/build" });
        let benign = json!({ "command": "git status" });

        let manual = BashTool::for_test().with_permission(BashPermissionMode::Manual);
        assert_eq!(manual.accessed_resources(&destructive).len(), 1);
        assert_eq!(manual.accessed_resources(&benign).len(), 1);

        let auto = BashTool::for_test().with_permission(auto_permission());
        assert!(auto.accessed_resources(&destructive).is_empty());
        assert!(auto.accessed_resources(&benign).is_empty());

        let free = BashTool::for_test().with_permission(BashPermissionMode::Free);
        assert!(free.accessed_resources(&destructive).is_empty());
        assert!(free.accessed_resources(&benign).is_empty());
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

        // Wrapper chains where the wrapped command is benign — the
        // descent resolves the real command position and checks only that
        // argv0, so the wrapper's own arguments never trip detection.
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

    #[test]
    fn contains_delete_command_catches_side_channels_and_wrapper_options() {
        // Deletes that run through a side channel — process substitution,
        // an expandable redirection / heredoc / here-string, or a command
        // substitution inside `[[ … ]]` — never appear in the argv but
        // still execute, so they must fire.
        for cmd in [
            "cat <(rm -rf build)",
            "diff <(cat a) <(rm -rf b)",
            "cat > >(rm -rf build)",
            "cat <<EOF\n$(rm -rf build)\nEOF",
            "grep x <<< $(rm -rf build)",
            "[[ -n $(rm -rf build) ]]",
            "[[ $(rm x) == y ]]",
            // sudo option that takes a separate value must not hide the
            // wrapped command behind it.
            "sudo --user root rm -rf build",
            "sudo --user=root rm -rf build",
            // process substitution / heredoc feeding a compound command's
            // own redirection, not a simple command's
            "while read x; do :; done < <(rm -rf build)",
            "{ ls; } > >(rm -rf build)",
            // a coprocess runs its body command
            "coproc rm -rf build",
            "coproc grp { rm -rf build; }",
            // compound headers are expanded before the body runs
            "for f in $(rm -rf build); do :; done",
            "case $(rm -rf build) in x) :; esac",
            "case x in $(rm -rf build)) :; esac",
            // command substitution nested inside a parameter / arithmetic
            // expansion (not a top-level `$(…)`)
            "echo ${UNSET:-$(rm -rf build)}",
            "echo \"${UNSET:-$(rm -rf build)}\"",
            ": $(( $(rm -rf build) ))",
            // xargs deprecated optional-operand flags: rm is the command
            "xargs -i rm",
            "xargs -l rm",
            // exec with a custom argv0 still runs the wrapped command
            "exec -a custom rm -rf build",
            // env's value-taking argv0 override hides the command otherwise
            "env -a custom rm -rf build",
            // arithmetic expressions expand command substitutions first
            "(( $(rm -rf build) ))",
            "for (( i=$(rm -rf build); i<1; i++ )); do :; done",
            // the command supplied as a string argument (env -S / shell -c /
            // eval) is parsed, not skipped as an opaque value
            "env -S rm -rf build",
            "env -S 'rm' -rf build",
            "env -S \"rm -rf build\"",
            "env --split-string='rm -rf build'",
            "sh -c \"rm -rf build\"",
            "bash -c 'ls; rm -rf build'",
            "/bin/sh -c 'rm -rf build'",
            "eval rm -rf build",
            "eval \"rm -rf build\"",
        ] {
            assert!(
                contains_delete_command(cmd),
                "side-channel delete must trigger detection in {cmd:?}"
            );
        }

        // Command substitution nested deeper than the recursion budget must
        // fail closed (prompt) rather than be silently ignored.
        let deep = format!("{}rm -rf x{}", "$(".repeat(40), ")".repeat(40));
        assert!(
            contains_delete_command(&deep),
            "deeply nested substitution must fail closed"
        );

        // Benign counterparts must NOT trigger: a harmless process subst, a
        // quoted heredoc (literal, never executed), a plain test, and a
        // wrapper whose value-taking option is followed by a benign command.
        for cmd in [
            "cat <(ls build)",
            "cat <<'EOF'\n$(rm -rf build)\nEOF",
            "[[ -f build ]]",
            "sudo --preserve-env ls",
            "sudo --user root ls",
            "for f in a b c; do :; done",
            "case x in y) :; esac",
            "echo ${HOME:-/tmp}",
            ": $(( 1 + 2 ))",
            "(( 1 + 2 ))",
            "xargs -i ls",
            "exec -a custom ls",
            "env -a custom ls",
            // command-string interpreters wrapping a benign command
            "env -S 'ls -la'",
            "sh -c \"grep rm file\"",
            "bash -c 'echo rm'",
            "eval echo rm",
        ] {
            assert!(
                !contains_delete_command(cmd),
                "benign side-channel command must NOT trigger in {cmd:?}"
            );
        }
    }

    #[test]
    fn contains_delete_command_ast_regressions() {
        // Real deletes hidden by constructs the old flat tokenizer
        // mangled (multi-line scripts, control flow, subshells, command
        // substitution, `find -exec` through a wrapper) MUST still fire.
        for cmd in [
            "cd build\nrm -rf *",
            "if [ -d x ]; then rm -rf x; fi",
            "if [[ -d x ]]; then rm -rf x; fi",
            "for f in a b; do rm \"$f\"; done",
            "while read f; do rm \"$f\"; done",
            "(cd x && rm -rf y)",
            "echo \"$(rm -rf build)\"",
            "x=$(rm -rf build)",
            "`rm -rf build`",
            "find . -type f -exec sudo rm {} \\;",
        ] {
            assert!(
                contains_delete_command(cmd),
                "expected delete detection for {cmd:?}"
            );
        }

        // False positives the substring / flat-token detector produced: a
        // delete token that is only an argument, a lookup query, or a
        // flagged no-op. None of these delete anything, so none may prompt.
        for cmd in [
            // delete token is an argument to a builtin/wrapper, not the argv0
            "command -v rm",
            "command -v shred",
            "type rm",
            "sudo pacman -S wipe",
            "sudo apt-get install srm",
            "sudo mv rm rm.old",
            "find . | xargs grep rm",
            "timeout 5 grep rm log",
            // `-delete` as a literal argument, not a `find` primary
            "echo -delete",
            "grep -- -delete file",
            // git rm variants that never touch the working tree
            "git rm --cached path",
            "git rm --dry-run file",
            "git rm -n file",
            // a quoted-heredoc body is opaque data, never parsed as commands
            "python - <<'PY'\nimport re\nx = re.compile(r'rm -rf /')\nprint('done')\nPY",
        ] {
            assert!(
                !contains_delete_command(cmd),
                "benign command must NOT trigger detection in {cmd:?}"
            );
        }
    }

    #[tokio::test]
    async fn direct_execute_does_not_consult_pre_execution_approval_gate() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: b"clean\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        // The pre-execution gate is wired through the registry, not
        // BashTool::execute itself, so no prompt fires here directly even
        // though manual permission declares an ExecCommand resource.
        let resources =
            BashTool::for_test().accessed_resources(&json!({ "command": "git status" }));
        assert_eq!(resources.len(), 1);

        let out = BashTool::for_test()
            .execute(json!({ "command": "git status" }), &ctx)
            .await
            .expect("benign command must run");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(
            gate.requests().is_empty(),
            "direct execute must not pop any in-flight approval prompt: {:?}",
            gate.requests()
        );
    }

    #[tokio::test]
    async fn unsandboxed_runner_exports_pwd_from_effective_cwd() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let process_manager = baybo_process::ProcessManager::transient();
        let out = run_unsandboxed(
            &process_manager,
            "env",
            &[],
            Some(workspace.path()),
            &[],
            Duration::from_secs(5),
        )
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

    #[tokio::test]
    async fn unsandboxed_runner_injects_extra_env() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let extra = [(
            "BAYBO_TEST_SECRET".to_string(),
            "injected-value".to_string(),
        )];
        let process_manager = baybo_process::ProcessManager::transient();
        let out = run_unsandboxed(
            &process_manager,
            "env",
            &[],
            Some(workspace.path()),
            &extra,
            Duration::from_secs(5),
        )
        .await
        .expect("env must run");
        let stdout = String::from_utf8(out.stdout).expect("env stdout");
        assert!(
            stdout
                .lines()
                .any(|l| l == "BAYBO_TEST_SECRET=injected-value"),
            "injected env var must reach the child:\n{stdout}"
        );
    }
}
