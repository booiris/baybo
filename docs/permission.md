# Bash Permission Policy

## Scope

`permission` is the top-level `baybo.json` field that controls Bash approval
and OS-sandbox usage:

```json
{
  "permission": "auto"
}
```

Accepted values are:

| Value | Meaning |
| --- | --- |
| `auto` | Default. Judge risky deletes and sandbox escapes, prompt only when the judge cannot approve automatically. |
| `manual` | Human approval before every executable Bash command, and again before any unsandboxed retry. |
| `free` | Run Bash directly without Bash approval or the OS sandbox. |

`open` and `none` are legacy aliases for `free`.

`permission` only controls the Bash tool. Other tools keep their own
validation and approval rules.

MCP tools are outside it in both directions: they never consult `permission`,
and as of the lazy-loading change they raise no approval prompt at all — see
[`todo/mcp-tool-approval.md`](todo/mcp-tool-approval.md) for why, and for what
still gates them (trust, channel, trigger scope, and the exact grant an
unattended cron lineage needs).

## Execution Model

The first route for `auto` and `manual` is the OS sandbox when an inner sandbox
runner is available. The sandbox uses the permissive filesystem policy described
in [`modules/sandbox.md`](modules/sandbox.md): the workspace and `$HOME` are
visible, sensitive credential directories are masked, and network is enabled for
Bash.

`free` runs without the OS sandbox, but it is not the same as the bench profile:
the Bash tool still enforces its work-directory jail and still keeps the uv
Python shim. The `bench-bash` Cargo feature is separate and stronger: it runs
raw in a disposable benchmark container, with no OS sandbox, no uv shim, no
work-directory jail, and inherited cwd.

### Per-call `sandbox_permissions`

The Bash tool has a separate per-call `sandbox_permissions` parameter:

| Value | Meaning |
| --- | --- |
| `use_default` | Default. Follow the configured `permission` route described below. |
| `require_escalated` | Use only when the user explicitly asks for this command to run without the OS sandbox. When the configured route is sandboxed, show the exact command in a fresh approval prompt, then run unsandboxed only if approved. Under `permission=free`, the route is already unsandboxed and no prompt is shown. |

Calls using `require_escalated` may also provide `justification`, a concise
user-facing question or reason shown at the top of the approval prompt. Empty
or omitted input uses a generic question. The exact command and the fixed
warning about normally hidden host files and credentials are always rendered
separately, so model-authored justification cannot hide what is being approved.

Under `permission=auto` or `manual`, `require_escalated` is an explicit privilege
transition even when an identical sandboxed command was approved earlier. Its
approval is therefore uncached: a session-level `ApproveAlways` entry for
`ExecCommand { command }` cannot silently promote that command to the host
route, and choosing “always” on this prompt does not grant later unsandboxed
calls. A denied, timed-out, abandoned, or unattended request starts no process.
Under `permission=free`, no privilege transition occurs, so
`require_escalated` and `justification` are ignored and no approval is raised.
The tool-layer work-directory jail, uv shim, timeout/background handling,
secret injection, and command-shape checks still apply; only the OS sandbox
route changes.

The bench-only `bench-bash` profile is already unsandboxed and intentionally
has no approval gate, so the parameter is a no-op there.

### `auto`

`auto` is the default policy for normal interactive use.

Before execution, Bash parses the command into a bash AST (via `brush_parser`)
and checks whether any command in it is a destructive file delete or a
destructive history- or remote-changing `git` operation. It matches the
resolved command-position argv0 of every simple command — including ones nested
in pipelines, `&&`/`||` lists, subshells, `if`/`for`/`while`/`case` bodies,
functions, and `$(…)`/backtick substitutions — after skipping wrapper argv0s
(`xargs`, `sudo`, `timeout`, `env`, …) and `KEY=VAL` env prefixes. The delete
argv0s are `rm`, `rmdir`, `unlink`, `shred`, `srm`, `wipe`, plus `find -delete`
/ `find -exec rm`. Because matching is argv0-anchored, a delete word that is
only a grep pattern, a filename operand, or a heredoc body does not trip the
gate. This AST scan is only a fast pre-filter: if it matches, the LLM judge
decides whether the command may proceed automatically.

- Judge says safe: run under the active route.
- Judge says risky, fails, or is unavailable: ask the user through the approval
  gate.
- No destructive token: run without a pre-execution prompt.

When the active route is sandboxed and the command fails, `auto` asks the judge
whether the failure looks sandbox-related and whether an unsandboxed retry is
safe.

- Sandbox-related and safe: rerun the same command outside the OS sandbox
  automatically.
- Sandbox-related but risky, or the judge is unavailable: ask for approval
  before the unsandboxed retry. On approval it reruns unsandboxed; on denial the
  original sandboxed failure is returned.
- Not sandbox-related — a compile error, a failing test, a network error, a
  deliberate non-zero exit: return the failure untouched, with no prompt. The
  escape prompt grants full host access, so it must not fire on failures the
  sandbox had no part in; that would reduce it to a reflex click.
- No approval handle for a verdict that needs one, such as cron or an
  unattended nested run: return the original sandboxed failure.

One correction rides on top of that verdict. A program the sandbox never
mounted fails with exactly the `exit 127` / `No such file or directory` a
genuinely deleted binary produces, and the judge reads the second: in this
repo's trace history it answered `sandbox_related: false` on four of five such
failures. So when the sandbox backend can *prove* the named program was outside
its mounts, that fact overrides a `false` verdict — but only as far as the
approval prompt. A judge that did not see the sandbox itself never causes an
unattended host rerun; reaching `Unsandbox` still requires the judge's own
`sandbox_related` verdict.

An unsandboxed retry after a command failure records `sandbox_escalation` in
the tool result, and every unsandboxed escape emits a warning notice when a
notifier is available. Independently of any retry, a sandboxed `exit 127` whose
program is provably unmounted records `sandbox_visibility` in the tool result,
naming the path and stating that the exit code is not evidence about whether
the file exists on the host.

### `manual`

`manual` sends every executable Bash command through the human approval gate
before it runs. After approval, the command runs in the OS sandbox when a runner
is available.

If the sandboxed command fails, Bash asks again before retrying outside the OS
sandbox. This second approval is intentionally separate from the first approval:
an unsandboxed retry has a wider privilege boundary than the original sandboxed
run.

File-reading and file-editing shell commands follow the same route as every
other Bash command. In `manual`, commands such as `cat foo` and `sed -i ...`
therefore require approval and then execute in the sandbox; Baybo does not
force the model to replace them with `Read` or `Edit`.

### `free`

`free` bypasses Bash approval and the OS sandbox. It is intended for trusted
hosts or containers where the outer environment is already the isolation
boundary.

The Bash tool still applies its normal tool-layer guards. In particular, cwd
and any absolute command-path argument must remain inside the configured work
directory (the caller's own read-only skill directory excepted) unless the build uses the
bench-only `bench-bash` feature.

## Sandbox Availability

At gateway boot, Baybo probes the OS sandbox backend once and stores the result
on `ToolExecutor`.

If Baybo detects that it is already running inside a common outer container or
sandbox, such as Docker, Kubernetes, Podman/containerd, LXC,
Singularity/Apptainer, or a generic container runtime (a `/run/.containerenv`
marker), it does not attempt to nest the inner OS sandbox. Bash
runs under the configured approval policy but without the inner sandbox, and the
user-facing downgrade notice is suppressed.

If no sandbox backend is available on a non-container host, Baybo keeps running.
Bash receives a `sandbox_bypass_reason` in `ToolContext`; before running without
the inner OS sandbox it emits a user-facing warning notice. The approval policy
still applies:

- `auto` still judges destructive commands and sandbox-escape decisions where
  applicable.
- `manual` still asks before each command.
- `free` still runs directly.

Future `ExecCommand` tools must choose their own downgrade or refusal semantics.
The Bash behavior is not an executor-wide fallback rule.

## Known Limitations

There is no immutable host-provided flag that proves Baybo is already running
inside a sandbox. The outer-container check is best-effort: it looks at marker
files and cgroup strings for common runtimes. Those signals can be absent,
spoofed, or ambiguous. Treat the detection as an operational convenience for
avoiding unsupported nested sandboxing, not as a security proof.

When no sandbox backend is installed on a non-container host, `auto` and
`manual` do not hard-fail Bash. They run without the inner OS sandbox after the
configured approval flow and a user-facing warning notice. This keeps Baybo
usable, but it means `permission = auto` and `permission = manual` are not
guarantees of OS isolation unless `bwrap`, `sandbox-exec`, or Docker is actually
available and warmed successfully.

The destructive-command detector used by `auto` parses the command into a bash
AST (`brush_parser`) and matches destructive commands at their resolved argv0;
it is not a complete shell interpreter. It handles the supported wrappers and
known destructive forms, but it does not resolve variables, aliases, globs, or
`$PATH` (so e.g. `CMD=rm; $CMD file` is not detected), and it is not a formal
proof that a command cannot delete or rewrite data. When a command cannot be
parsed, the detector fails closed to a substring keyword check. Use
`permission = manual` when the operator wants every Bash command to pass through
human approval regardless of classification.

## Hot Reload

`permission` is hot-reloadable. `hot_reload_diff` allows a config reload that
changes only this field, and the runtime reloader updates a shared
`LivePermissionMode` handle used by the Bash registry entry.

The next command and the next tool description observe the new permission. A
command already in progress keeps the permission snapshot it received at
`execute` entry, so a mid-command reload cannot change the escalation policy for
that command.

## Approval Ownership

The executor approval gate and Bash's internal judge intentionally do not both
own the same prompt.

In `manual`, Bash declares an `ExecCommand` resource for every executable Bash
command using `sandbox_permissions=use_default`, so the executor approval gate
prompts before execution.

In `auto`, Bash declares no execution resource at all — neither for destructive
nor benign commands. For a destructive command Bash owns the pre-execution judge
and, when needed, asks the approval gate itself; benign commands run unprompted.
This avoids double prompts for the same command.

In `free`, Bash declares no execution resource because the mode is explicitly
ungated.

For `sandbox_permissions=require_escalated`, Bash declares no pre-execution
resource in any global permission mode. Under `auto` or `manual`, it owns one
uncached mid-execution prompt instead, whose `ExecCommand` access and preview
both name the exact command. This avoids a double prompt in `manual` and
prevents the ordinary resource cache from collapsing the sandboxed and
unsandboxed privilege levels. Under `free`, it raises no prompt because the
command already runs outside the OS sandbox.

## Background Runs

`on_timeout=background` can detach both sandboxed and unsandboxed commands.
Sandboxed commands hand the sandbox backend's detached child to the background
job sink. Unsandboxed routes, including `permission = free` and Baybo
self-invocations, spawn the child in its own process group so cancellation can
stop the whole tree.

Commands with `secret_env` may background, but output files and completion
notification tails are raw. The handoff returns a warning when secret env vars
were injected.

## Key Code Paths

- `crates/config/src/permission.rs`: config enum and serde aliases.
- `crates/config/src/lib.rs`: top-level `BayboConfig.permission`.
- `crates/baybo/src/boot.rs`: maps config `PermissionPolicy` to Bash's internal
  enum.
- `crates/baybo/src/reload.rs`: hot-reloads `LivePermissionMode`.
- `crates/baybo/src/sandbox_boot.rs`: sandbox backend probing,
  outer-container bypass detection, and downgrade-reason selection.
- `crates/baybo/src/runtime.rs`: injects the sandbox boot result into
  `ToolExecutor`.
- `crates/tools/src/builtin/bash/mod.rs`: execution routing, approval
  integration, sandbox fallback, and background handling.
- `crates/tools/src/builtin/bash/parse.rs`: destructive-command AST detector
  (`contains_delete_command`), wrapper/argv0 resolution, and the `git`/`find`
  destructive-form matchers.
- `crates/tools/src/builtin/bash/judge.rs`: auto-permission risk judge prompts
  and verdict parsing.
