# sandbox - OS-Level Tool Isolation

## Overview

The `sandbox` crate (`baybo-sandbox`) provides per-invocation OS-native
isolation for tools that shell out. Today every tool runs in-process inside
the gateway; without the sandbox, `BashTool` (the only `ExecCommand`-capable
builtin) would spawn `sh -c <user-supplied>` with the gateway's full
privileges. With the sandbox enabled, the spawn is wrapped by the platform's
native isolation primitive: `bwrap` on Linux, `sandbox-exec` on macOS.

What the crate provides:

- A `SandboxRunner` trait and a `current_platform_runner()` factory that
  returns the right backend for the current `target_os`.
- A `SandboxSpec` describing the program, workspace root, readable
  paths, extra `writable_paths`, network policy, environment policy,
  stdin source, timeout, `resource_limits` (memory + pids caps), and
  the `FilesystemPolicy` selecting between strict workspace-only writes
  and the permissive "host RW + denylist" model used by `BashTool`.
- `cfg`-free `args.rs` that renders the bwrap argv and the SBPL profile,
  so spec → invocation is unit-testable on any developer host.
- An `baybo_sandbox::probe()` entry point so callers (the gateway) can detect
  a missing backend at startup and react cleanly. The `bootstrap` module
  exports the `SandboxAvailability` struct that `probe()` returns; the
  function itself sits at the crate root.

What gets wrapped:

- Tools whose manifest declares `baybo_tools::ToolCapability::ExecCommand`.
  The `ToolExecutor` builds a `SandboxAdapter` per call and injects it into
  `ToolContext.sandbox`; the tool then routes its child through
  `ExecSandbox::spawn_command(program, args, SpawnOpts { cwd, stdin, extra_env, timeout })`.
  `extra_env` injects per-call `KEY=value` pairs (e.g. secrets resolved for a Bash
  `secret_env`) that the adapter maps to `EnvPolicy::BaselineWithExtra` — emitted via
  bwrap `--setenv` / macOS `env` args, never through the command string. In-process
  Rust tools (Read, Write, Edit, Glob, Grep, Now) are unchanged — their syscalls
  happen inside the gateway and there is nothing for the sandbox to enforce.

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
pattern. Baybo is Unix-only and Windows is out of scope.

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
- **Symlinked cwd spelling**: when a caller supplies an explicit `cwd`
  whose canonical target is inside `SandboxSpec.workspace_root`, the
  backend also exposes that requested spelling inside the sandbox
  (`source = cwd.canonicalize()`, `destination = cwd`). The crate-public
  `workspace_symlink_mount_for` helper computes this `WorkspaceSymlinkMount`
  (both re-exported from the crate root); each backend consumes it. This
  is a convenience mount only; the security decision stays anchored on the
  canonical workspace root. The sandbox does not parse arbitrary shell
  command strings to discover path spellings.
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
  unique `--name baybo-sandbox-<pid>-<nanos>-<seq>`. On timeout or
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
  Operators who need caps in daemonised setups should run baybo
  under a systemd user unit or use the Docker fallback.
- **macOS (sandbox-exec)**: SBPL has no cgroup equivalent. Default
  is `unlimited()`. Non-`unlimited()` specs return
  `SandboxError::Unenforceable`. Operators who need a hard memory
  ceiling on macOS should layer a separate launchd
  `MemoryHighWaterMark` outside baybo.

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

- `ToolCapability::ExecCommand` ⇒ wrap. The adapter is built with
  `FilesystemPolicy::Permissive` (see "Filesystem policy" below) and
  `NetworkPolicy::All`. Today only `BashTool` declares `ExecCommand`;
  shells almost always need git/cargo/npm reachable, so the policy is
  baked in rather than gated on a separate capability.
- Tools without `ExecCommand` get no sandbox (their syscalls happen
  in-process inside the gateway and there is nothing for the runner
  to enforce).

`FilesystemPolicy::Workspace` (the historical "deny by default" model
described below) currently has no production consumer — Bash is the
only `ExecCommand` tool today and it always opts into `Permissive`.
The variant remains on `SandboxSpec` for future tools that want a
per-call deny-by-default scope (e.g. an LLM-driven script runner).

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

### Filesystem policy: `Workspace` vs. `Permissive`

`SandboxSpec.filesystem_policy` selects between two models. Callers
that don't set it inherit `FilesystemPolicy::Workspace` (backward
compatible default).

**`Workspace`** — the historical strict model (no current production
consumer; retained for future per-call deny-by-default tools).
Only `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`, and
`/run/systemd/resolve` are RO-bound (via `--ro-bind-try` so missing
entries don't fail the call). `workspace_root` is the only RW host
path. Anything outside is invisible; the bwrap tmpfs `/tmp` is the
only other writable surface. macOS SBPL mirrors this: deny default,
allow read on a curated FHS-mac equivalent + workspace, allow write
only on workspace + writable_paths.

**`Permissive { extra_root, denied_paths }`** — the model used for
`BashTool`. The agent's RW surface is exactly `workspace_root +
extra_root` (the agent layer defaults `extra_root` to `$HOME`).
`extra_root` is mounted with `--bind-try` so a missing `$HOME` (e.g.
on minimal containers) downgrades silently instead of failing the
call. FHS roots (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`,
`/run/systemd/resolve`) stay RO-bound so installed binaries and
resolv.conf still work. Anything outside that union is *not* visible
inside the sandbox — there is **no full host-root bind**. `--proc`,
`--dev`, and `--tmpfs /tmp` overlay a fresh PID namespace, a minimal
devtmpfs (no host raw devices like `/dev/sda`), and a per-call temp
dir. Each entry in `denied_paths` is then masked with an empty
per-call `tmpfs`, so credential vaults that physically sit inside
`extra_root` look empty inside the sandbox regardless of what was on
the host. The OS user's own permission bits stay in effect on top.
macOS SBPL emits a read-allow block over the FHS-mac equivalent +
`workspace_root` + `extra_root`, a write-allow block over
`workspace_root` + `extra_root`, then `(deny file-read*/file-write*
(subpath …))` per denied path; SBPL's last-match-wins evaluation
produces the same observable shape as the bwrap policy.

`SandboxAdapter::with_permissive_filesystem(extra_root, denied_paths)`
is the agent-side opt-in. It also relaxes the "cwd must be inside
workspace_root" check, since the writable surface is now wider than
`workspace_root`. Non-existent denied paths are filtered at adapter
build time so bwrap never sees a `--tmpfs <missing>` line.

`SandboxAdapter::with_readable_paths(paths)` adds **read-only** re-binds
on top of that policy (filtered to existing paths at build time). The
agent layer passes `<workspace>/skills` here: the denylist masks all of
`~/.baybo`, but installed skill scripts must still be executable in
place, so `skills/` is re-bound RO *after* the masking tmpfs — the same
last-wins ordering that re-establishes the `work/` dir, except RO. On
bwrap that's `--ro-bind-try <skills> <skills>` emitted after the
`--tmpfs` masks; on macOS SBPL it's an `(allow file-read* (subpath …))`
emitted after the `(deny …)` block (no `file-write*` re-allow, so writes
still `EPERM`); Docker already binds `readable_paths` as `-v …:ro`. The
companion guard in `BashTool` (`require_command_paths_within_work_dir`)
mirrors this: a command-argument path under `skills/` is accepted even
though it sits outside `work/`, while `cwd` stays pinned to `work/`.

Default Bash denylist (built by
`baybo_sandbox::default_sensitive_denylist`):

- `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.gpg` — credentials and keys
- `~/.config/gh`, `~/.config/gcloud` — cloud CLI tokens
- `~/.docker`, `~/.kube` — registry / cluster auth
- `$BAYBO_HOME` (or `~/.baybo` if unset) — Baybo's own state, secrets,
  identity files. The whole tree is masked, then `skills/` alone is
  re-exposed read-only via `with_readable_paths` (above) so skill
  scripts run in place; `config/`, `state/`, `profile/`, `.key/`, etc.
  stay hidden.

Trade-offs:

- `Permissive`'s blast radius is "everything under `workspace_root +
  $HOME` the OS user can write to, minus the masked credential
  vaults". `/etc/sudoers`, `/var/log`, `/srv`, `/data` (when not the
  workspace), … stay invisible. Pair with the per-command approval
  gate (which already fires on `rm`/destructive `git`) and the
  `baybo_security` content filters for defence in depth.
- On non-FHS distros (NixOS, GoboLinux) the FHS-RO bind list still
  leaves `/nix/store` uncovered; if the agent needs binaries from
  there, prefer launching baybo with the workspace inside `$HOME` so
  the home bind covers it, or extend `BWRAP_RO_ROOTS`.
- Docker ignores the permissive *denylist / extra_root* policy and
  binds only `workspace_root` (plus `readable_paths` RO, so `skills/`
  still works) — reaching `$HOME` from inside a container would need an
  explicit bind that defeats the container model.

### Sandbox FS root vs. Baybo state directory

These are **different paths**, on purpose:

- `baybo_workspace::WorkspaceManager.root` is `config.workspace.path`
  (defaults to `~/.baybo`). It's where Baybo keeps its libsql storage,
  identity files, lock file, and skill bundles. The agent reads from
  here in-process, not via subprocesses.
- The sandbox FS root passed to `ToolExecutor::new` is the *project
  workspace*: where the user expects shell commands to run. The
  gateway resolves this from `std::env::current_dir().canonicalize()`
  at startup — the directory baybo was launched from. If
  `current_dir()` fails, the gateway falls back to the state
  directory and logs a warning.
- `SandboxAdapter` rejects any `cwd` that isn't a subpath of the
  sandbox FS root (after canonicalization on both sides, so macOS's
  `/tmp` → `/private/tmp` symlink doesn't trip false escape errors).

Daemon-launched setups whose `current_dir()` is `/` will fall back to
the state directory, which is almost certainly not what the operator
wants — that case will be addressed by the deferred `[sandbox]` config
section.

### Tempdir routing

`$TMPDIR` / `$TMP` / `$TEMP` are always set to the in-sandbox `/tmp`,
but what `/tmp` actually points at depends on the filesystem policy:

- **`Workspace`** (no current production consumer): `/tmp` is a fresh
  per-call scratch — `--tmpfs /tmp` on bwrap, `tempdir_in(workspace_root)`
  on macOS with `TMPDIR` pointed at it. The macOS scratch dir is removed
  when the call returns (best-effort — `kill -9` of the gateway
  leaves it behind, recognizable by the `.baybo-sandbox-` prefix). The
  SBPL profile still denies writes to host `/tmp` / `/private/tmp`,
  so a script that ignores `$TMPDIR` and hardcodes `/tmp` gets `EPERM`
  from the kernel on macOS; on Linux it writes into the per-call tmpfs
  which is disposed on exit. Either way, nothing leaks out of the call.

- **`Permissive`** (used by `BashTool`): `/tmp` is the host's real
  `/tmp` — bwrap binds it with `--bind /tmp /tmp`; sandbox-exec adds
  `/private/tmp` to the SBPL file-write\* allow block and points
  `TMPDIR=/tmp` directly. Files written under `/tmp` persist across
  Bash calls because the host directory itself is the persistence
  mechanism — usually a kernel tmpfs cleared on reboot. There is **no
  per-session isolation**: every Bash call across every session sees
  the same `/tmp` as the host user and any other host process.
  Credential-vault denylist entries (`~/.ssh`, `~/.aws`, …) still
  mask in their original locations; `/tmp` is not on that list.

This is a deliberate trade-off — session-scoped persistence with
per-call namespacing was rejected because the bookkeeping (mkdir,
chmod, sanitization, cleanup policy that aligns with "session data is
core data") outweighed the isolation gain. Bash is already a shared,
shell-level escape hatch; treating its `/tmp` as host-shared matches
how a developer using `/tmp` from their own terminal already expects
it to behave.

Docker is unaffected: the Docker runner does not bind host `/tmp`
into the container — the image's default `/tmp` layer applies and is
disposed when the `--rm` container exits.

### DNS resolution under `--share-net`

When network is allowed, the sandbox needs to resolve hostnames. Binding
`/etc` RO gives the child `nsswitch.conf` and `resolv.conf`;
`/run/systemd/resolve` is bound for systems using systemd-resolved's stub
resolver. Bind-mounting `/etc/resolv.conf` directly is avoided because
some setups make it a dangling symlink.

### Bootstrap probe at startup, not per-call

`baybo_sandbox::current_platform_runner()` is called once during gateway
boot. The result is stored on `ToolExecutor`; per-call cost is just an
`Arc::clone`. A missing backend at startup becomes a single error log
plus `sandbox_runner: None`, which `ToolExecutor` checks per-tool: if the
tool needs the sandbox and the runner is absent, the call is refused with
an actionable error. This keeps the rest of the gateway running normally.

### Runner injected via `ToolContext`, not interposed by the executor

The `ExecSandbox` trait lives in `baybo-tools`. `baybo-agent` defines a
`SandboxAdapter` that implements `ExecSandbox` and wraps a
`Arc<dyn baybo_sandbox::SandboxRunner>`. The executor builds one adapter per
call (when needed) and attaches it to `ToolContext.sandbox`. The tool then
opts in by calling `ctx.sandbox.spawn_command(...)`.

This keeps the `Tool` trait shape unchanged and avoids `baybo-tools`
depending on `baybo-sandbox`. Future ExecCommand-capable tools get the
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

- Linux + macOS only. On an unsupported target both
  `current_platform_runner()` and `probe()` return
  `SandboxError::NoBackendAvailable`.
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
- MCP stdio servers (`baybo-tools::mcp::McpReconciler`) currently spawn
  outside the sandbox. Wrapping them is on the deferred list.

## Collaboration

| Module       | Role                                                                                          |
|--------------|-----------------------------------------------------------------------------------------------|
| `tools`      | Defines `ExecSandbox` trait + `SandboxedOutput` and adds `sandbox` / `workspace_root` to `ToolContext`. `BashTool` opts in to routing.   |
| `agent`      | Builds `SandboxAdapter` per call; passes the runner into `ToolExecutor::new`; refuses ExecCommand tools when no runner is configured. |
| `security`   | Hosts the decision-layer primitives (SSRF resolution in `WebFetch::validate_url_with`, leak detection, secret vault). The sandbox is the enforcement layer that makes those decisions real for ExecCommand tools. |
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
