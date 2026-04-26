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

### Backend per platform, with a Docker fallback

`#[cfg(target_os = "linux")]` selects `BwrapRunner`; `#[cfg(target_os =
"macos")]` selects `SandboxExecRunner`. Both are gated behind matching
Cargo features (`linux`, `macos`), mirroring the gateway's installer
pattern. Aura is Unix-only and Windows is out of scope.

`current_platform_runner()` tries the native backend first; if its
binary is missing on `$PATH`, it falls back to `DockerRunner` (gated
on the `docker` feature, also default-on). All three features ship in
the default set so the same build works whether the operator has
`bwrap`, only `docker`, or — on a fresh macOS — only the OS-bundled
`sandbox-exec`. If none are available the factory returns
`SandboxError::NoBackendAvailable` and the gateway logs an error +
refuses every `ExecCommand` call.

The three backends have nothing meaningful in common at the syscall
level — bwrap namespaces vs. SBPL rules vs. dockerd-managed cgroups —
so the trait surface is intentionally minimal: `run(spec) ->
Result<SandboxOutput, SandboxError>` and `backend()` for diagnostics.
There is no shared "policy" type beyond `SandboxSpec` itself.

#### Docker fallback specifics

- **Lifecycle**: `discover()` (sync) verifies the docker CLI is on
  `$PATH` and that `docker info` returns successfully — i.e. the
  daemon socket is reachable and accessible to the gateway user. A
  binary without a reachable daemon returns
  `SandboxError::BackendUnreachable` and the factory keeps falling
  through (or reports `NoBackendAvailable`).
- **`warm()` at startup**: the Docker backend overrides the default
  no-op `SandboxRunner::warm()`. It calls `docker image inspect` for
  the configured image; if absent, runs `docker pull` once; then
  resolves the image to its digest reference (`debian@sha256:…`) and
  stores it in a `OnceLock`. Every subsequent `docker run` uses that
  digest, with `--pull=never`, so the trust boundary is fixed for
  the gateway's lifetime — even if the floating tag is rotated
  upstream mid-session.
- **Default image**: `debian:stable-slim`. Hardcoded for v1; a
  `[sandbox]` config section will let operators override it.
- **Workspace bind**: `-v <workspace>:<workspace>` with the same path
  inside and outside the container so absolute paths line up for the
  `cwd` validation in `SandboxAdapter` and for any path the tool
  emits.
- **Network**: `--network none` for `NetworkPolicy::None`, `--network
  bridge` for `All`. We avoid `--network host` because Docker Desktop
  on macOS does not expose host networking — `bridge` works
  identically on Linux and macOS.
- **User**: `--user <host-uid>:<host-gid>` so files the child writes
  back into the workspace bind are owned by the launching user, not
  root.
- **Capabilities**: `--cap-drop=ALL`. The container kernel surface is
  whatever the docker daemon chooses to expose, which is narrower
  than a bwrap-namespaced child but not zero.
- **Resource caps**: `--memory <bytes> --pids-limit <count>` are
  emitted whenever `SandboxSpec.resource_limits` carries those
  bounds (default 512 MiB / 256 pids). Setting either to `None` —
  via `ResourceLimits::unlimited()` — drops the corresponding flag,
  in which case the container inherits the daemon's defaults.
- **Container lifecycle**: every `docker run` is started with a
  unique `--name aura-sandbox-<pid>-<nanos>-<seq>`. On timeout or
  io-error the runner issues `docker rm -f <name>` before returning,
  because `kill_on_drop` only reaps the local docker CLI client and
  the daemon-managed container would otherwise keep running with
  the workspace bind still mounted. There is a regression test in
  `tests/docker_smoke.rs::timeout_force_removes_container` that
  proves a timed-out workload cannot continue writing into the
  workspace.
- **Daemon-side launch failures** (exit 125 with stderr) are
  surfaced as `SandboxError::BackendFailure` so the agent doesn't
  mis-attribute them to the user command.

### Resource caps (memory + pids)

`SandboxSpec.resource_limits` carries `memory_max_bytes` and
`pids_max`, both `Option<u64>`. The library default is
`ResourceLimits::unlimited()` — most permissive — so a `SandboxSpec`
constructed with `..Default::default()` works on every backend.
`ResourceLimits::safe_defaults()` (512 MiB / 256 processes) is the
conservative recommendation; backends that can enforce it use it as
their `default_resource_limits`.

Per-runner enforcement (queryable through
`SandboxRunner::default_resource_limits`):

- **Docker**: direct mapping to `--memory <bytes>` / `--pids-limit
  <count>`. Always enforceable, so the runner's default is
  `safe_defaults()`.
- **bwrap with `systemd-run --user`** (probed once at `discover()`
  via `systemd-run --user --scope -- /bin/true`): caps go through
  cgroup v2 by wrapping the bwrap call in `systemd-run --user
  --scope --collect --property=MemoryMax=… --property=TasksMax=…`.
  Default is `safe_defaults()`.
- **bwrap without `systemd-run`** (daemonised hosts, containers
  without user systemd): cgroup caps are unreachable, so the
  runner's default is `unlimited()`. If a caller explicitly passes
  non-`unlimited()` limits, `BwrapRunner::run` returns
  `SandboxError::Unenforceable` rather than silently downgrading.
  Operators who need caps in daemonised setups should run aura
  under a systemd user unit or use the Docker fallback.
- **macOS (sandbox-exec)**: SBPL has no cgroup equivalent. Default
  is `unlimited()`. Non-`unlimited()` specs return
  `SandboxError::Unenforceable`. Operators who need a hard memory
  ceiling on macOS should layer a separate launchd
  `MemoryHighWaterMark` outside aura.

`SandboxAdapter::new` reads `runner.default_resource_limits()` so the
agent's per-call default is automatically tuned to whatever the
chosen backend can enforce. Callers can still tighten or relax via
`with_resource_limits(...)`; the runner validates and either
enforces or fails-closed.

The fail-closed posture is the headline change in the v1.1 hardening
pass: prior to it, both Linux without `systemd-run` and macOS
silently dropped the request, so `OK(...)` did not actually mean the
caps were applied. Any caller asking for caps now either gets them
or gets a clear error telling them which backend can deliver.

### Capability-driven wrapping

The `ToolExecutor` decides whether a tool needs the sandbox by checking
its manifest:

- `ToolCapability::ExecCommand` ⇒ wrap.
- `ToolCapability::Http` (paired with `ExecCommand`) ⇒
  `NetworkPolicy::All`; otherwise `NetworkPolicy::None`.

`Http` is broad-allow when `SandboxSpec.allowed_hosts` is empty: the
tool gets `--share-net` (Linux) / `--network bridge` (Docker) /
`(allow network*)` (macOS) and can talk to anything the host can
reach. When `allowed_hosts` is non-empty the runners diverge sharply:

- **macOS**: the SBPL profile flips to `(deny network*)` followed by
  one `(allow network-outbound (remote tcp "host:port"))` line per
  entry, plus DNS over UDP and TCP port 53 so hostname rules can
  resolve. Bare hostnames default to `host:*` (any port). The kernel
  sandbox enforces this for **all TCP traffic** — there is no in-tree
  way to bypass it short of the agent breaking out of the sandbox
  entirely.
- **Linux (bwrap)**: `allowed_hosts` is currently **advisory**. The
  runner emits a `tracing::warn!` so operators see the field is
  ignored, then runs with the regular `--share-net` (full host
  network) for `NetworkPolicy::All`. The earlier in-process CONNECT
  proxy was removed in the v1.1 hardening pass — its bypass surface
  (raw TCP, plain HTTP, anything that ignored `HTTPS_PROXY`) made
  the "best-effort" gate dishonest. The proper kernel-level
  replacement (pre-provisioned netns + iptables egress filter) is
  scoped in `docs/todo/sandbox-os-isolation.md` Path A but deferred.
  Until that lands, callers who depend on host scoping should use
  the macOS backend.
- **Docker**: same as bwrap — `allowed_hosts` is **advisory**. The
  runner emits a `tracing::warn!` and runs with the regular bridge
  network. Docker has no in-tree per-host enforcement either; the
  proper fix (a routable filter that the container can't bypass)
  lives in `docs/todo/sandbox-os-isolation.md` next to the bwrap
  netns work and is deferred to the same future round.

`SandboxSpec.allowed_hosts` is still in the public API and wired to
`SandboxAdapter::with_allowed_hosts(...)` so:

- macOS callers can populate it and get real enforcement.
- Linux/Docker callers immediately surface the limitation rather
  than learning later that the boundary was advisory.
- A future netns + nftables enforcer (or a kernel-level Linux
  alternative) can plug into the same field without an API change.

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
- Per-host network allowlist enforces only on macOS (SBPL, all TCP).
  bwrap and Docker both accept the field as advisory (warn + ignore).
  The proper kernel-level Linux/Docker enforcer is deferred — see
  `docs/todo/sandbox-os-isolation.md`.
- Resource caps enforce on Docker (always) and bwrap (when
  `systemd-run --user` is usable). Non-`unlimited()` caps on
  sandbox-exec or bwrap-without-`systemd-run` return
  `SandboxError::Unenforceable`; `SandboxAdapter` defers to the
  runner's `default_resource_limits()` so default-policy callers
  pick limits the backend can actually deliver.
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

- **Per-host network allowlist on Linux** (bwrap and Docker). The
  macOS SBPL path shipped. Linux currently fail-closes; the future
  enforcement layer is netns + nftables egress filtering on bwrap
  (CAP_NET_ADMIN inside the user namespace makes this tractable
  without privileged setup), and a container-reachable filter for
  Docker (host-gateway routing or in-container sidecar). Until that
  lands, callers wanting host scopes use macOS or run with empty
  `allowed_hosts`.
- MCP stdio sandboxing.
- `[sandbox]` config section for timeouts, memory caps, extra
  readable paths, and an explicit override for the sandbox FS root.
- Configurable Docker image (currently hardcoded to
  `debian:stable-slim`) with a digest-pinned default in source so the
  trust boundary is reproducible across fresh hosts.
