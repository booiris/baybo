# Sandbox — OS-Level Tool Isolation

## v1 Status (shipped)

- `aura-sandbox` crate with `bwrap` (Linux), `sandbox-exec` (macOS),
  and a cross-platform `docker` fallback, all behind matching Cargo
  features (default-on).
- `SandboxRunner` trait + `current_platform_runner()` factory that
  tries the native backend first and falls back to docker when its
  binary is absent. Per-call cost is just an `Arc::clone` after the
  startup probe.
- Filesystem scoping: workspace bind mount RW; curated system dirs
  (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`,
  `/run/systemd/resolve`) RO.
- Network: per-call all-or-nothing — `ToolCapability::Http` ⇒
  `--share-net` / `(allow network*)`, otherwise `--unshare-net` /
  `(deny network*)`.
- Bootstrap probe at gateway startup; missing backend ⇒ ExecCommand
  tools refuse with an actionable error, the rest of the gateway runs
  normally.
- `BashTool` routes through `ToolContext.sandbox`; in-process Rust
  tools (Read, Write, Edit, Glob, Grep, Now) are untouched.

Module spec: [`docs/modules/sandbox.md`](../modules/sandbox.md).

## Deferred (post-v1)

- **MCP stdio servers** (`aura-tools::mcp::transport::connect_stdio`,
  exercised by `McpReconciler`). The reconciler spawns the configured
  binary directly via `tokio::process::Command` — outside the sandbox —
  even though stdio MCP servers are functionally `ExecCommand`-class
  subprocesses with the same blast radius as `BashTool`. The
  `ToolExecutor`-side capability check does not help here: the spawn
  happens at registration, before any tool call, and `McpTool::execute`
  never consults `ToolContext.sandbox`. **Path forward**: move the
  spawn site itself behind `aura_sandbox::SandboxRunner` rather than
  trying to retrofit per-call sandboxing — the long-lived stdio
  transport doesn't fit the per-invocation model and needs a
  long-running variant of the runner. Until that lands, treat stdio
  MCP servers as trusted extensions and refuse to register them when
  the runner is unavailable.
- `socat`-driven per-host network allowlist on Linux; SBPL host-scoped
  network rules on macOS. Today `SandboxSpec.allowed_hosts` is
  forward-compatible plumbing; v1 ignores its contents.
- cgroup v2 memory + pid caps (Linux). v1 only sets `--die-with-parent`
  + a fresh PID namespace. The Docker fallback similarly lacks
  `--memory` / `--pids-limit` flags and inherits the daemon's
  defaults.
- Configurable Docker image (currently hardcoded to
  `debian:stable-slim`). The runner already pre-pulls the image at
  startup via `SandboxRunner::warm()` and pins it by digest for the
  gateway lifetime, so the first agent-issued command no longer
  pays the registry round-trip; what is left is exposing the image
  choice to operators (and shipping a pinned-by-digest default in
  source so the trust boundary is reproducible across fresh hosts).
- `[sandbox]` config section in `aura.json` for timeouts, memory caps,
  extra readable paths (especially relevant for non-FHS distros like
  NixOS where `/usr` is essentially empty), and an explicit override
  for the sandbox FS root (currently inferred from `current_dir()` at
  startup; useful when aura is launched from a daemon manager whose
  cwd is `/`).

## Original Problem

The previous `aura-sandbox` crate (WASM-based, `wasmtime`-backed) was
removed because the abstraction was premature: it forced every tool
through a WASM runtime while the real isolation need is per-tool-
invocation OS-level containment for shell/filesystem/network side
effects. A WASM runtime does not help with that — it sandboxes compute,
not the host syscalls tools actually make.

Today `ToolExecutor` runs tools directly in-process. Without OS-native
isolation, any tool that shells out runs with the full privileges of
the `aura` process.

## Direction

Rebuild the sandbox as an OS-native isolation layer, selected per
platform. Tool manifests declare the capabilities they need
(`aura_tools::ToolCapability`); the executor wraps the tool invocation
in the platform's native sandbox with exactly those capabilities
granted.

### Linux — `bwrap` (+ future `socat`)

- **`bwrap`** provides the core isolation: new user/mount/pid/uts/ipc
  namespaces, RO bind of curated system dirs (`/usr`, `/bin`, `/sbin`,
  `/lib`, `/lib64`, `/etc`, `/run/systemd/resolve`), a tmpfs `/tmp`,
  and an RW bind of the tool's workspace root. `--unshare-net` by
  default; selectively `--share-net` for tools whose manifest declares
  `ToolCapability::Http`.
- **`socat`** (deferred) bridges allow-listed network endpoints into
  the sandbox. When a tool's `Http` capability scopes to specific
  hosts, spin up a `socat` proxy on a unix socket inside the sandbox
  that forwards only to the declared host:port. Enforcement lives at
  `socat` configuration time — the sandbox's network namespace has no
  other route out.
- Resource limits via `bwrap --die-with-parent` + cgroup v2 (memory,
  pids) — cgroup caps are deferred.

### macOS — `sandbox-exec`

- `sandbox-exec -p '<sbpl-profile>' <cmd>` with a generated Sandbox
  Profile Language policy per invocation. Default-deny, then allow:
  - `file-read*` on the curated system dirs + workspace + declared
    readable paths.
  - `file-write*` on the workspace root only. Host temp directories
    (`/private/tmp`, `/private/var/folders`) are denied; the runner
    creates a per-invocation scratch dir under the workspace and
    points `$TMPDIR` / `$TMP` / `$TEMP` at it so callers respecting
    those env vars stay inside the bind.
  - `network*` toggled by `NetworkPolicy`. Future: SBPL `(remote ip
    "host:port")` style rules for per-host scoping.

### Shared surface (implemented)

- `aura-sandbox` exposes `SandboxRunner::run(spec)` returning a handle
  with stdout/stderr/exit. Implementation is `cfg(target_os)` gated:
  `linux` → bwrap backend, `macos` → sandbox-exec backend. Other
  platforms return `SandboxError::UnsupportedPlatform`.
- `aura_tools::ToolCapability` (`ReadFile`, `WriteFile`, `Http`,
  `ExecCommand`) drives wrapping. The `ToolExecutor` consults the
  runner only for tools whose manifest declares `ExecCommand`.
  In-process Rust tools continue to run directly — sandboxing
  in-process code is what WASM was for and we removed that.

## Open Questions (still relevant)

- **DNS resolution**: with `--unshare-net` we don't need DNS at all.
  With `--share-net` we bind `/etc` and `/run/systemd/resolve` RO.
  When per-host `socat` lands, hostname resolution will move into the
  host namespace and IPs get pre-resolved — document the TTL vs.
  re-resolution tradeoff at that point.
- **Process tree**: tools can fork inside the sandbox; bwrap caps the
  blast radius via PID namespace + `--die-with-parent`.
- **Windows**: deferred — no planned support, `current_platform_runner()`
  returns `SandboxError::UnsupportedPlatform`.
- **Bootstrap check**: implemented — `aura_sandbox::probe()` and
  `current_platform_runner()` return `SandboxError::BackendMissing`
  with an actionable install hint when the binary is absent.
- **Config surface**: a `[sandbox]` section is deferred and would land
  in `config-wire-remaining-sections.md` when it's needed.

## Related

- [`docs/modules/sandbox.md`](../modules/sandbox.md) — module spec.
- [`docs/modules/tools.md`](../modules/tools.md) —
  `ToolManifest.capabilities` is the input the runner consumes.
- [`docs/modules/security.md`](../modules/security.md) — when a network
  policy decision layer is added, sandbox is the enforcement layer
  below it.
