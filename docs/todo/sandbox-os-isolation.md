# Sandbox — OS-Level Tool Isolation

## Problem

The previous `aura-sandbox` crate (WASM-based, `wasmtime`-backed) was removed
because the abstraction was premature: it forced every tool through a WASM
runtime while the real isolation need is per-tool-invocation OS-level
containment for shell/filesystem/network side effects. A WASM runtime does not
help with that — it sandboxes compute, not the host syscalls tools actually
make.

Today `ToolExecutor` runs tools directly in-process. There is no filesystem
scoping, no network allow-list enforcement at the OS layer, and no way to cap a
tool's syscall surface. Any tool that shells out runs with the full privileges
of the `aura` process.

## Proposed Direction

Rebuild the sandbox as an OS-native isolation layer, selected per platform.
Tool manifests declare the capabilities they need; the executor wraps the tool
invocation in the platform's native sandbox with exactly those capabilities
granted.

### Linux — `bubblewrap` + `socat`

- **`bwrap`** provides the core isolation: new user/mount/pid/uts/ipc
  namespaces, read-only bind of the host root (or a minimal `/usr`), a
  tmpfs `/tmp`, and a scoped read-write bind of the tool's workspace root.
  `--unshare-net` by default; selectively keep networking for tools that
  declare `Http(..)` capability.
- **`socat`** bridges allow-listed network endpoints into the sandbox. When a
  tool declares `Http(host)`, spin up a `socat` proxy on a unix socket inside
  the sandbox that forwards only to the declared host:port. Enforcement lives
  at `socat` configuration time — the sandbox's network namespace has no other
  route out.
- Resource limits via `bwrap --die-with-parent` + cgroup v2 (memory, pids).
  No CPU cap by default; add `cpu.max` only if a tool tag opts in.

### macOS — `sandbox-exec`

- `sandbox-exec -p '<sbpl-profile>' <cmd>` with a generated Sandbox Profile
  Language policy per invocation. Default-deny, then allow:
  - `file-read*` on the tool's declared read set.
  - `file-write*` on the workspace root (scoped).
  - `network*` only for hosts declared by `Http(..)` capability (SBPL supports
    `(remote ip "*:443")` style rules, scoped to hostnames that the tool
    resolves to prior to invocation, or to a local proxy on 127.0.0.1).
- No `socat` bridge needed on macOS — SBPL's `network*` rules are enforced in
  the kernel.

### Shared surface

- A new `aura-sandbox` crate exposes `SandboxRunner::spawn(cmd, manifest)`
  returning a handle with stdout/stderr/exit. Implementation is `cfg(target_os)`
  gated: `linux` → bwrap backend, `macos` → sandbox-exec backend. Other
  platforms return an "unsupported" error and the tool must be refused.
- `ToolManifest.capabilities` (already defined in `aura-tools`) is the input:
  `ReadWorkspace`, `WriteWorkspace`, `Http(host)`, `SpawnProcess`. The runner
  maps each to the platform-native primitive.
- `ToolExecutor` consults the runner only for tools tagged `external`
  (shell-backed or binary-backed). In-process Rust tools continue to run
  directly — sandboxing in-process code is what WASM was for and we just
  removed that.

## Open Questions

- **DNS resolution**: with `--unshare-net`, how does `socat` resolve hostnames?
  Likely pre-resolve in the host namespace and pass IPs into the sandbox;
  document the TTL vs. re-resolution tradeoff.
- **Process tree**: do we allow tools to spawn children? Default no; `bwrap`
  already caps this via pid namespace + `--die-with-parent`.
- **Windows**: deferred — no planned support, error out cleanly.
- **Bootstrap check**: at startup verify `bwrap`/`sandbox-exec` binaries are
  present and executable; fail fast with an actionable message if not.
- **Config surface**: needs a new `sandbox` section for timeouts, memory caps,
  and workspace scope rules. Would land in `config-wire-remaining-sections.md`
  after this design stabilizes.

## Related

- `docs/modules/tools.md` — `ToolManifest.capabilities` is the input the
  runner consumes.
- `docs/modules/security.md` — `NetworkPolicyDecider` is the permission
  decision layer; sandbox is the enforcement layer below it.
- `docs/todo/config-wire-remaining-sections.md` — future `sandbox` config
  section lands there once this design is firm.
