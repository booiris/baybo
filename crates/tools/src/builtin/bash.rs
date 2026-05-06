//! `Bash` — execute a shell command via `sh -c` inside the OS sandbox.
//!
//! Every shell-needing command runs through bwrap (Linux) /
//! sandbox-exec (macOS) / docker. The sandbox runs in **permissive
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
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ApprovalDecision, ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_KIB: usize = MAX_OUTPUT_BYTES / 1024;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Execute a shell command in a fresh `sh -c` process. Environment \
         changes and `cd` do not persist across invocations. Each of \
         stdout and stderr is truncated at {MAX_OUTPUT_KIB} KiB.\n\n\
         IMPORTANT: Do NOT use Bash for tasks that have a dedicated tool:\n\
         - File-content viewers (`cat`, `head`, `tail`, `less`, `more`, \
         `tac`) and file-driven text processors (`sed`, `awk`) are \
         REJECTED at this layer when invoked as the leading command — \
         use `Read` for content (`offset`/`limit` cover head/tail), \
         `Edit` for in-place changes (safer than `sed -i`). \
         Stream-mode `sed`/`awk` AFTER a pipe is fine (e.g. \
         `git log | sed 's/.../.../'`) — only `sed <file>` / `awk <file>` \
         is blocked.\n\
         - To write files use `Write` (not echo/cat with redirection)\n\
         - To search file names use `Glob` (not find/ls)\n\
         - To search file contents use `Grep` (not grep/rg)\n\n\
         SANDBOX: The shell runs with read+write access to the project \
         workspace and `$HOME` (FHS roots `/usr`, `/bin`, `/etc`, … \
         stay readable; nothing outside that union is visible — no \
         full host-root bind). Credential vaults inside `$HOME` \
         (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.gpg`, `~/.config/gh`, \
         `~/.config/gcloud`, `~/.docker`, `~/.kube`, and the Aura \
         state dir under `~/.aura`/`$AURA_HOME`) are masked with empty \
         tmpfs and look empty inside the sandbox. Host raw devices \
         stay unreachable (`/dev` is a minimal devtmpfs). Network is \
         enabled.\n\n\
         APPROVAL: sandboxed commands run without prompting by default. \
         The pre-execution approval gate only fires when the command \
         tokens contain a file-delete (`rm`, `rmdir`, `unlink`, `shred`, \
         `srm`, `wipe`, or `find … -delete`) or a destructive `git` \
         operation (`clean -f`, `reset --hard`, `branch -d`/`-D`/\
         `--delete`, `tag -d`, `push -f`/`--force`/`--force-with-lease`/\
         `--delete`/`-d`, `stash drop`/`clear`, `worktree remove`, \
         `update-ref -d`, `filter-branch`, `filter-repo`); a separate \
         prompt also fires if the sandbox refuses the command and an \
         unsandboxed retry is available.\n\
         Reserve Bash for system commands, git operations, build/test, \
         and terminal tasks that require shell execution.\n\n\
         DEFAULT CWD: If `cwd` is omitted, Aura runs the command from the \
         workspace work directory and exports `PWD` with the same value.\n\n\
         PATHS: Any directory or file argument inside the command (cd, ls, \
         mkdir, rm, mv, cp, find, …) MUST be an absolute path. The optional \
         `cwd` parameter MUST also be absolute when provided — relative \
         values are rejected. Always quote file paths that contain spaces \
         with double quotes (e.g. `cd \"/path with spaces/file.txt\"`).\n\n\
         BEFORE BROAD SCANS: Do not run `find`, `du`, recursive `ls`, or \
         similar walks against unknown directories without first checking \
         their size with a bounded probe (e.g. `ls -1 <dir> | wc -l`, or a \
         shallow `find -maxdepth 2`). Large trees can hang the process."
    )
});

pub struct BashTool;

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

    fn description(&self) -> &str {
        &DESCRIPTION
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
        }

        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.timeout);

        let command = p.command;
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

        let Some(sandbox) = ctx.sandbox.as_ref() else {
            return Err(ToolError::Execution(
                "OS sandbox unavailable: install bwrap (Linux: `apt install bubblewrap`) \
                 or sandbox-exec (macOS, ships with the system) and restart aura"
                    .into(),
            ));
        };

        let args = vec!["-c".into(), command.clone()];
        let attempt = tokio::select! {
            _ = ctx.cancellation_token.cancelled() => {
                return Err(ToolError::Execution("cancelled".into()));
            }
            res = sandbox.spawn_command(Path::new("sh"), &args, cwd_ref, None, timeout) => res,
        };
        let out = match attempt {
            Ok(out) => out,
            Err(sandbox_err) => {
                // Sandbox infrastructure refused the command (cwd
                // outside the bound union, bwrap setup failure, runner
                // error, …). Offer the user a one-shot unsandboxed
                // retry that surfaces the failure reason in the
                // approval prompt.
                prompt_and_run_unsandboxed_retry(&command, cwd_ref, timeout, ctx, sandbox_err)
                    .await?
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

async fn prompt_and_run_unsandboxed_retry(
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
            let args = ["-c".to_string(), command.to_string()];
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

    fn ctx_with(sandbox: Option<Arc<dyn crate::ExecSandbox>>) -> ToolContext {
        ToolContext {
            session_id: "t".into(),
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
        let err = BashTool
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
        let out = BashTool
            .execute(json!({ "command": "echo hello" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("hello"));

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, std::path::PathBuf::from("sh"));
        assert_eq!(
            calls[0].args,
            vec!["-c".to_string(), "echo hello".to_string()]
        );
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
        BashTool
            .execute(json!({ "command": "pwd" }), &ctx_with(Some(sandbox)))
            .await
            .expect("pwd must run");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "pwd must consult the sandbox now");
        assert_eq!(calls[0].args, vec!["-c".to_string(), "pwd".to_string()]);
    }

    #[tokio::test]
    async fn surfaces_non_zero_exit_from_sandbox() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 7,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
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
        let err = BashTool
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
        let err = BashTool
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
    async fn grep_exit_one_reports_no_matches() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
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
        let out = BashTool
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
        let out = BashTool
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
        let out = BashTool
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

        let out = BashTool
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

        let err = BashTool
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

        let err = BashTool
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
        let out = BashTool
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

        let out = BashTool
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
        let out = BashTool
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
        let resources = BashTool.accessed_resources(&json!({ "command": "cat foo" }));
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
            let resources = BashTool.accessed_resources(&json!({ "command": cmd }));
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
            let resources = BashTool.accessed_resources(&json!({ "command": cmd }));
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
            let label = BashTool.call_label(&json!({ "command": cmd }));
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
                BashTool.call_label(&json!({ "command": cmd })),
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
            let err = BashTool
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
        let resources = BashTool.accessed_resources(&json!({ "command": "git status" }));
        assert!(resources.is_empty());

        let out = BashTool
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
