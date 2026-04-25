# sandbox - OS-Level Tool Isolation

## Overview

The `sandbox` crate (`aura-sandbox`) provides per-invocation OS-native
isolation for tools that shell out. Today every tool runs in-process inside
the gateway; without the sandbox, `BashTool` (the only `ExecCommand`-capable
builtin) would spawn `sh -c <user-supplied>` with the gateway's full
privileges. With the sandbox enabled, the spawn is wrapped by the platform's
native isolation primitive: `bwrap` on Linux, `sandbox-exec` on macOS.

What the crate provides:

- A `SandboxRunner` trait and a `current_platform_runner()` factory that
  returns the right backend for the current `target_os`.
- A `SandboxSpec` describing the program, workspace root (the only
  filesystem path the child can write to), readable paths, network policy,
  environment policy, stdin source, and timeout.
- `cfg`-free `args.rs` that renders the bwrap argv and the SBPL profile,
  so spec → invocation is unit-testable on any developer host.
- A `bootstrap::probe()` entry point so callers (the gateway) can detect a
  missing backend at startup and react cleanly.

What gets wrapped:

- Tools whose manifest declares `aura_tools::ToolCapability::ExecCommand`.
  The `ToolExecutor` builds a `SandboxAdapter` per call and injects it into
  `ToolContext.sandbox`; the tool then routes its child through
  `ExecSandbox::spawn_command`. In-process Rust tools (Read, Write, Edit,
  Glob, Grep, Now) are unchanged — their syscalls happen inside the gateway
  and there is nothing for the sandbox to enforce.

If the backend binary is missing at startup, the gateway logs an error and
runs with `sandbox_runner = None`; any subsequent `ExecCommand` tool call
returns `ToolError::Execution("OS sandbox unavailable: …")`. Other tools
continue to function. This is a deliberate refuse-don't-fall-back: silently
running unsandboxed would defeat the whole point.

## Design Decisions

### Backend per platform, not a shared abstraction

`#[cfg(target_os = "linux")]` selects `BwrapRunner`; `#[cfg(target_os =
"macos")]` selects `SandboxExecRunner`. Both are gated behind matching
Cargo features (`linux`, `macos`, both on by default), mirroring the
gateway's installer pattern. Other Unixes return
`SandboxError::UnsupportedPlatform` — Aura is Unix-only and Windows is
out of scope.

The two backends have nothing meaningful in common at the syscall level —
namespace flags vs. SBPL rules — so the trait surface is intentionally
minimal: `run(spec) -> Result<SandboxOutput, SandboxError>` and `backend()`
for diagnostics. There is no shared "policy" type beyond `SandboxSpec`
itself.

### Capability-driven wrapping

The `ToolExecutor` decides whether a tool needs the sandbox by checking
its manifest:

- `ToolCapability::ExecCommand` ⇒ wrap.
- `ToolCapability::Http` (paired with `ExecCommand`) ⇒
  `NetworkPolicy::All`; otherwise `NetworkPolicy::None`.

`Http` is currently all-or-nothing: a tool that needs network gets
`--share-net` (Linux) or `(allow network*)` (macOS), no host-level
allowlist. A future iteration adds per-host scoping via `socat` (Linux)
and SBPL `(remote ip …)` rules (macOS); the `SandboxSpec.allowed_hosts`
field exists for forward compatibility but is currently unused. See
`docs/todo/sandbox-os-isolation.md`.

### Curated read-only mounts (vs. a full host-root bind)

The original design proposal said "RO bind of the host root (or a minimal
`/usr`)". The implementation chose the minimal-curated approach: only
`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`, and
`/run/systemd/resolve` are RO-bound, all via `--ro-bind-try` so missing
paths don't fail the call. A literal `--ro-bind / /` would expose `/home`,
`/root`, and `/var`, defeating the workspace-only write scope.

Trade-off: on non-FHS distros (NixOS, GoboLinux) the curated list won't
cover `/nix/store`, so most binaries won't actually run. A future
configurable extra-readable-paths field is on the deferred list.

### Sandbox FS root vs. Aura state directory

These are **different paths**, on purpose:

- `aura_workspace::WorkspaceManager.root` is `config.workspace.path`
  (defaults to `~/.aura`). It's where Aura keeps its libsql storage,
  identity files, lock file, and skill bundles. The agent reads from
  here in-process, not via subprocesses.
- The sandbox FS root passed to `ToolExecutor::new` is the *project
  workspace*: where the user expects shell commands to run. The
  gateway resolves this from `std::env::current_dir().canonicalize()`
  at startup — the directory aura was launched from. If
  `current_dir()` fails, the gateway falls back to the state
  directory and logs a warning.
- `SandboxAdapter` rejects any `cwd` that isn't a subpath of the
  sandbox FS root (after canonicalization on both sides, so macOS's
  `/tmp` → `/private/tmp` symlink doesn't trip false escape errors).

Daemon-launched setups whose `current_dir()` is `/` will fall back to
the state directory, which is almost certainly not what the operator
wants — that case will be addressed by the deferred `[sandbox]` config
section.

### Tempdir routing (writes outside the workspace)

`SandboxSpec.workspace_root` is the only host path the child may write
to (plus the bwrap tmpfs `/tmp` on Linux). To keep callers that respect
`$TMPDIR` / `$TMP` / `$TEMP` from blowing up, the runner sets those
variables explicitly:

- **Linux**: `TMPDIR=/tmp`, `TMP=/tmp`, `TEMP=/tmp`. The bwrap-mounted
  tmpfs at `/tmp` is fresh per invocation and disposed when bwrap
  exits.
- **macOS**: the runner creates a `.aura-sandbox-XXXXXX` directory
  under `workspace_root` for each call and points the temp env vars at
  it. The SBPL profile allows writes only inside the workspace, so
  even programs that ignore `$TMPDIR` and hard-code `/tmp` or
  `/private/var/folders` will get `(deny)` from the kernel. The
  scratch directory is removed when the call returns (best-effort —
  `kill -9` of the gateway leaves the directory behind, recognizable
  by the `.aura-sandbox-` prefix).

### DNS resolution under `--share-net`

When network is allowed, the sandbox needs to resolve hostnames. Binding
`/etc` RO gives the child `nsswitch.conf` and `resolv.conf`;
`/run/systemd/resolve` is bound for systems using systemd-resolved's stub
resolver. Bind-mounting `/etc/resolv.conf` directly is avoided because
some setups make it a dangling symlink.

### Bootstrap probe at startup, not per-call

`aura_sandbox::current_platform_runner()` is called once during gateway
boot. The result is stored on `ToolExecutor`; per-call cost is just an
`Arc::clone`. A missing backend at startup becomes a single error log
plus `sandbox_runner: None`, which `ToolExecutor` checks per-tool: if the
tool needs the sandbox and the runner is absent, the call is refused with
an actionable error. This keeps the rest of the gateway running normally.

### Runner injected via `ToolContext`, not interposed by the executor

The `ExecSandbox` trait lives in `aura-tools`. `aura-agent` defines a
`SandboxAdapter` that implements `ExecSandbox` and wraps a
`Arc<dyn aura_sandbox::SandboxRunner>`. The executor builds one adapter per
call (when needed) and attaches it to `ToolContext.sandbox`. The tool then
opts in by calling `ctx.sandbox.spawn_command(...)`.

This keeps the `Tool` trait shape unchanged and avoids `aura-tools`
depending on `aura-sandbox`. Future ExecCommand-capable tools get the
runner for free without further executor changes.

### `cfg`-free argv/profile rendering

`args::build_bwrap_argv` and `args::render_sbpl_profile` are pure
functions over `SandboxSpec`. They have no `cfg(target_os)` gating, so
their unit tests run on any host — covering the parts most likely to
break (argument ordering, network gate selection, SBPL escaping). The
backend-specific glue (`bwrap.rs`, `sandbox_exec.rs`) is `cfg`-gated and
exercised by the smoke tests in `tests/`, which skip cleanly when the
backend binary is absent.

## Constraints

- Linux + macOS only. Other targets receive
  `SandboxError::UnsupportedPlatform`.
- v1 network gate is per-call all-or-nothing. Per-host scoping is
  deferred.
- No cgroup memory or pid caps in v1; `--die-with-parent` plus a fresh
  PID namespace are the only resource controls.
- ExecCommand tools refuse to run when the backend binary is missing —
  no fallback to unsandboxed execution.
- MCP stdio servers (`aura-tools::mcp::McpReconciler`) currently spawn
  outside the sandbox. Wrapping them is on the deferred list.

## Collaboration

| Module       | Role                                                                                          |
|--------------|-----------------------------------------------------------------------------------------------|
| `tools`      | Defines `ExecSandbox` trait + `SandboxedOutput` and adds `sandbox` / `workspace_root` to `ToolContext`. `BashTool` opts in to routing.   |
| `agent`      | Builds `SandboxAdapter` per call; passes the runner into `ToolExecutor::new`; refuses ExecCommand tools when no runner is configured. |
| `security`   | The decision layer (e.g. `NetworkPolicyDecider`, when added) sits above the sandbox; the sandbox is the enforcement layer that makes the decision real. |
| `bootstrap`  | `src/runtime.rs` calls `current_platform_runner()` at startup and threads the result into `ToolExecutor`.                              |

## Deferred (post-v1)

Tracked in [`docs/todo/sandbox-os-isolation.md`](../todo/sandbox-os-isolation.md):

- `socat`-driven per-host network allowlist on Linux; SBPL host-scoped
  network rules on macOS.
- cgroup v2 memory and pid caps (Linux).
- MCP stdio sandboxing.
- `[sandbox]` config section for timeouts, memory caps, and extra
  readable paths.
