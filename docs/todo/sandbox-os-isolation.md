# Sandbox — OS-Level Tool Isolation

## v1 Status (shipped)

- `baybo-sandbox` crate with `bwrap` (Linux), `sandbox-exec` (macOS),
  and a cross-platform `docker` fallback, all behind matching Cargo
  features (default-on).
- `SandboxRunner` trait + `current_platform_runner()` factory that
  tries the native backend first and falls back to docker when its
  binary is absent. Per-call cost is just an `Arc::clone` after the
  startup probe.
- Filesystem scoping (`FilesystemPolicy::Permissive`): workspace root
  **and** `$HOME` bind-mounted RW; curated system dirs (`/usr`, `/bin`,
  `/sbin`, `/lib`, `/lib64`, `/etc`, `/run/systemd/resolve`) RO;
  credential vaults inside `$HOME` (`~/.ssh`, `~/.aws`, `~/.gnupg`,
  `~/.gpg`, `~/.config/gh`, `~/.config/gcloud`, `~/.docker`, `~/.kube`,
  the baybo state dir) masked with per-call empty tmpfs.
- Network: the crate supports per-call all-or-nothing
  (`NetworkPolicy::All` ⇒ `--share-net` / `(allow network*)`, `None` ⇒
  `--unshare-net` / `(deny network*)`); the `ToolExecutor` currently
  passes `NetworkPolicy::All` for every ExecCommand tool — the per-tool
  `ToolCapability::Http` gate described in the original design was not
  wired.
- **Resource caps** (`SandboxSpec.resource_limits` →
  `memory_max_bytes` + `pids_max`):
  - Library default is `ResourceLimits::unlimited()`;
    `safe_defaults()` (512 MiB / 256) is offered as a constant for
    callers and runners that can enforce it.
  - `SandboxRunner::default_resource_limits()` returns runner-aware
    safe caps: Docker = `safe_defaults()`; bwrap with `systemd-run`
    = `safe_defaults()`; bwrap without `systemd-run` =
    `unlimited()`; sandbox-exec = `unlimited()`. Callers like
    `SandboxAdapter::new` pull from this so per-call defaults track
    the backend's actual capability.
  - **Fail-closed on unenforceable specs**: `BwrapRunner::run`
    refuses non-`unlimited()` limits when `systemd-run` is missing,
    and `SandboxExecRunner::run` refuses any non-`unlimited()`
    spec, both with `SandboxError::Unenforceable`. No silent
    downgrade.
- **Per-host network allowlist**:
  - **macOS**: when `SandboxSpec.allowed_hosts` is non-empty under
    `NetworkPolicy::All`, the SBPL profile flips to `(deny network*)`
    plus DNS allows (UDP/TCP 53) plus one `(allow network-outbound
    (remote tcp "host:port"))` per entry. Bare hostnames default to
    `host:*`. Enforced by the kernel for all TCP traffic.
  - **Linux (bwrap)**: `allowed_hosts` is **advisory**. The runner
    logs a warn so operators see the field is ignored, then runs
    with `--share-net` for `NetworkPolicy::All`. The earlier
    "best-effort CONNECT proxy" was removed during the v1.1
    hardening pass — it gated HTTPS-via-CONNECT only and any direct
    `connect()` in the shared net namespace bypassed it. The proper
    kernel-level replacement is scoped in Deferred (Path A — pre-
    provisioned netns + iptables) and intentionally **not** added
    in this round, which is scoped to resource caps only.
  - **Docker**: same as bwrap — `allowed_hosts` is **advisory**,
    runner logs a warn and runs with the regular bridge network.
    Same enforcement design needed; same deferral.
- Bootstrap resolve at gateway startup (`resolve_sandbox_runner` in
  `crates/baybo/src/sandbox_boot.rs`): a missing/unusable backend records
  a bypass reason — Bash emits a one-time notice and runs without the
  inner OS sandbox (no refusal); inside a detected outer
  container/sandbox the inner sandbox is skipped silently; the rest of
  the gateway runs normally.
- `BashTool` routes through `ToolContext.sandbox`; in-process Rust
  tools (Read, Write, Edit, Glob, Grep, Now) are untouched.

Module spec: [`docs/modules/sandbox.md`](../modules/sandbox.md).

## Deferred (post-v1)

- **MCP stdio servers** (`baybo-tools::mcp::transport::connect_stdio`,
  exercised by `McpReconciler`). The reconciler spawns the configured
  binary directly via `tokio::process::Command` — outside the sandbox —
  even though stdio MCP servers are functionally `ExecCommand`-class
  subprocesses with the same blast radius as `BashTool`. The
  `ToolExecutor`-side capability check does not help here: the spawn
  happens at registration, before any tool call, and `McpTool::execute`
  never consults `ToolContext.sandbox`. **Path forward**: move the
  spawn site itself behind `baybo_sandbox::SandboxRunner` rather than
  trying to retrofit per-call sandboxing — the long-lived stdio
  transport doesn't fit the per-invocation model and needs a
  long-running variant of the runner. Until that lands, treat stdio
  MCP servers as trusted extensions and refuse to register them when
  the runner is unavailable.
- **Per-host network allowlist on Linux** (bwrap and Docker). The
  macOS SBPL path shipped. Linux currently warns and ignores
  `allowed_hosts` (advisory, warn-and-proceed with the broad allow — see
  the v1 Status bullet above), so operators who want host scopes on
  Linux have nothing yet. Any working solution must enforce at
  kernel level (catches HTTPS, plain HTTP, raw TCP, SSH, postgres,
  … without requiring tool cooperation), because anything that
  relies on tool cooperation has already been audited and rejected
  (see "Earlier prototype" below).

  ### Three viable paths, ordered by recommended priority

  #### Path A — pre-provisioned shared sandbox netns (recommended)

  Operator does the privileged work **once at install time**;
  runtime is zero-privilege. All sandboxes share one netns with a
  fixed allowlist read from a config file.

  Install-time setup script (root, runs once via systemd unit):

  ```bash
  ip netns add baybo-sandbox
  ip link add veth-baybo-h type veth peer name veth-baybo-s
  ip link set veth-baybo-s netns baybo-sandbox

  ip addr add 10.42.0.1/24 dev veth-baybo-h
  ip link set veth-baybo-h up
  sysctl -w net.ipv4.ip_forward=1
  ip netns exec baybo-sandbox ip addr add 10.42.0.2/24 dev veth-baybo-s
  ip netns exec baybo-sandbox ip link set veth-baybo-s up
  ip netns exec baybo-sandbox ip link set lo up
  ip netns exec baybo-sandbox ip route add default via 10.42.0.1

  iptables -t nat -A POSTROUTING -s 10.42.0.0/24 ! -d 10.42.0.0/24 -j MASQUERADE
  for ip in $(resolve /etc/baybo/sandbox-egress.conf); do
    iptables -A FORWARD -s 10.42.0.0/24 -d $ip -j ACCEPT
  done
  iptables -A FORWARD -s 10.42.0.0/24 -m conntrack \
      --ctstate ESTABLISHED,RELATED -j ACCEPT
  iptables -A FORWARD -s 10.42.0.0/24 -j DROP
  ```

  Runtime: baybo's bwrap children inherit the pre-built netns (no
  `--unshare-net`). The `setns()` privilege is owned by systemd via
  `JoinsNamespaceOf=baybo-sandbox-net.service` in `baybo.service`
  (so baybo itself stays unprivileged), or via a small file-cap
  helper (`setcap cap_sys_admin+ep`) if baybo is not deployed as a
  systemd service.

  - **Pros**: kernel line-rate; zero per-call setup cost; cleanup is
    a single static configuration; no new binary deps beyond
    `iproute2` + `iptables` (basically every distro).
  - **Cons**: `SandboxSpec.allowed_hosts` is no longer per-call —
    must be a subset of the static config or fail-closed; concurrent
    sandboxes share the netns (lo, conntrack, route table), so a
    long-running localhost server in sandbox A is reachable from
    sandbox B; doesn't work when baybo runs inside a container that
    can't help bridge to its host.
  - **API impact**: agent's intent (`SandboxSpec.allowed_hosts`)
    becomes a *declaration*; operator's policy
    (`/etc/baybo/sandbox-egress.conf`) is the *enforcement*. Runtime
    intersects: spec must be a subset of policy or fail-closed. This
    keeps two layers honest — code says what it wants, ops says
    what's allowed.

  #### Path B — per-call netns + slirp4netns + nftables

  Each sandbox call creates a fresh netns inside its user namespace,
  uses `slirp4netns` for connectivity (entirely userspace, no
  privilege), and installs `nft` rules inside the netns (where
  CAP_NET_ADMIN is real because we own the user namespace).

  - **Pros**: per-call `allowed_hosts` works; concurrent sandboxes
    fully isolated; truly zero-privilege at all stages; works in
    containers and locked-down deployments.
  - **Cons**: `slirp4netns` package dep (not on minimal Alpine);
    userspace TCP/IP stack adds ~1-3ms latency per packet and
    5-20% throughput hit; +50-200ms startup per call; per-call DNS
    pre-resolution + rule install/teardown adds moving parts.

  #### Path C — per-call veth + iptables MASQUERADE

  Classic container-runtime pattern. Per-call: create veth pair,
  move one end into sandbox netns, configure IP/routes, install
  per-call iptables FORWARD rules.

  - **Pros**: kernel-fast like Path A; per-call `allowed_hosts`
    works.
  - **Cons**: requires host-init `CAP_NET_ADMIN` per call (user
    namespace's "fake root" doesn't satisfy this — see below),
    which means setuid helper / sudo / privileged daemon — i.e.
    permanent privileged surface. iptables rule lifecycle is
    fragile: a crashed baybo leaves stale rules; concurrent calls
    race on rule numbering. Doesn't work in containers without
    additional plumbing.

  ### Why user-namespace CAP_NET_ADMIN isn't enough

  The user namespace bwrap creates gives the sandbox CAP_NET_ADMIN
  inside its own netns. But:
  - Creating veth pairs that live in init netns: requires init-ns
    CAP_NET_ADMIN.
  - Moving an interface across netns: same.
  - Writing iptables NAT/FORWARD rules in init netns (where the
    forwarded packets actually traverse): same.

  So per-call kernel-mode bridging always needs init-ns root. Path
  A pays it once at install; Path B avoids it entirely via
  userspace; Path C pays it on every call.

  ### Why not sudo
  Direct `NOPASSWD: /sbin/iptables, /sbin/ip` is unsafe: baybo runs
  LLM-driven agents, command injection upstream becomes host root.
  A wrapper script with sudoers is acceptable but is just
  privileged-helper-shaped-as-shell — at which point Path A's
  systemd unit is cleaner. `sudo` per-call also adds ~10-30ms PAM
  overhead per command, and ~10 commands per setup/teardown =
  100-300ms of pure sudo cost per sandbox call.

  ### Why not the previous CONNECT proxy
  An earlier prototype shipped a per-call HTTP CONNECT proxy bound
  to host loopback with `HTTPS_PROXY` env injection. It was removed
  in v1.1 once we audited the bypass surface: any tool that didn't
  honor `HTTPS_PROXY`, used plain HTTP (`HTTP_PROXY` was
  intentionally unset), or made raw TCP/UDP calls bypassed the
  proxy entirely. Since the sandbox runs with `--share-net` for
  `NetworkPolicy::All`, the kernel routes those bypassing connections
  directly to the host's network. The replacement must be
  kernel-level so it can't be sidestepped — which is what all three
  paths above achieve.

  ### Recommendation
  Start with Path A. It matches baybo's deployment reality (operator
  installs as a service, allowed-hosts list is relatively static
  per-deployment) and avoids the dependency / overhead drawbacks of
  Path B. Revisit Path B only if a real use case needs per-call
  scope variation or runs in a container that can't bridge to a
  host netns.

  Docker per-host enforcement is its own subtask. The same Path-A
  pattern adapts (operator pre-creates a docker network with
  filtered egress, baybo attaches every container to it), but
  Docker's daemon owns the bridge plumbing so the install script
  uses `docker network create` + `iptables` in `DOCKER-USER` chain
  instead of raw `ip` / `iptables`.
- Configurable Docker image (currently hardcoded to
  `debian:stable-slim`). The runner already pre-pulls the image at
  startup via `SandboxRunner::warm()` and pins it by digest for the
  gateway lifetime, so the first agent-issued command no longer
  pays the registry round-trip; what is left is exposing the image
  choice to operators (and shipping a pinned-by-digest default in
  source so the trust boundary is reproducible across fresh hosts).
- `[sandbox]` config section in `baybo.json` for timeouts, default
  resource limits (so operators can tighten or loosen the per-call
  defaults without editing source), and extra readable paths (especially
  relevant for non-FHS distros like NixOS where `/usr` is essentially
  empty). The sandbox FS root is already operator-controlled — it is
  `config.workspace.path` (absolute, validated; defaults to `~/.baybo` in
  release) — so no separate override is needed unless a deployment wants
  the sandbox root decoupled from the workspace root.

## Original Problem

The previous `baybo-sandbox` crate (WASM-based, `wasmtime`-backed) was
removed because the abstraction was premature: it forced every tool
through a WASM runtime while the real isolation need is per-tool-
invocation OS-level containment for shell/filesystem/network side
effects. A WASM runtime does not help with that — it sandboxes compute,
not the host syscalls tools actually make.

At the time of writing (pre-v1), `ToolExecutor` ran every tool directly
in-process, so any tool that shelled out ran with the full privileges of
the `baybo` process.

## Direction

Rebuild the sandbox as an OS-native isolation layer, selected per
platform. Tool manifests declare the capabilities they need
(`baybo_tools::ToolCapability`); the executor wraps the tool invocation
in the platform's native sandbox with exactly those capabilities
granted.

### Linux — `bwrap`

- **`bwrap`** provides the core isolation: new user/mount/pid/uts/ipc
  namespaces, RO bind of curated system dirs (`/usr`, `/bin`, `/sbin`,
  `/lib`, `/lib64`, `/etc`, `/run/systemd/resolve`), a tmpfs `/tmp`,
  and an RW bind of the tool's workspace root. `--unshare-net` by
  default; selectively `--share-net` for tools whose manifest declares
  `ToolCapability::Http`.
- Resource limits via `bwrap --die-with-parent` + cgroup v2 (memory,
  pids) — shipped via `systemd-run --user --scope` (see v1 Status
  above).

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
  - `network*` toggled by `NetworkPolicy`. Shipped: per-host scoping via
    `(allow network-outbound (remote tcp "host:port"))` rules — see the
    v1 Status per-host allowlist bullet.

### Shared surface (implemented)

- `baybo-sandbox` exposes `SandboxRunner::run(spec)` returning a handle
  with stdout/stderr/exit. Implementation is `cfg(target_os)` gated:
  `linux` → bwrap backend, `macos` → sandbox-exec backend. Other
  platforms fall through to the docker backend if available, else
  `SandboxError::NoBackendAvailable`.
- `baybo_tools::ToolCapability` (`ReadFile`, `WriteFile`, `Http`,
  `ExecCommand`) drives wrapping. The `ToolExecutor` consults the
  runner only for tools whose manifest declares `ExecCommand`.
  In-process Rust tools continue to run directly — sandboxing
  in-process code is what WASM was for and we removed that.

## Open Questions (still relevant)

- **DNS resolution**: with `--unshare-net` we don't need DNS at all.
  With `--share-net` we bind `/etc` and `/run/systemd/resolve` RO.
  When per-host enforcement lands on Linux (Path A), `allowed_hosts` get
  pre-resolved to IPs at install/config time — document the TTL vs.
  re-resolution tradeoff at that point.
- **Process tree**: tools can fork inside the sandbox; bwrap caps the
  blast radius via PID namespace + `--die-with-parent`.
- **Windows**: deferred — no planned support; `current_platform_runner()`
  returns `SandboxError::NoBackendAvailable` (after the docker fallback
  probe fails).
- **Bootstrap check**: implemented — `baybo_sandbox::probe()` and
  `current_platform_runner()` return `SandboxError::NoBackendAvailable`
  with an actionable install hint when the binary is absent (the
  per-backend `BackendMissing` errors are swallowed by the fallback
  chain; both variants carry actionable install hints).
- **Config surface**: a `[sandbox]` section is still deferred — see the
  Deferred list above for its intended contents.

## Related

- [`docs/modules/sandbox.md`](../modules/sandbox.md) — module spec.
- [`docs/modules/tools.md`](../modules/tools.md) —
  `ToolManifest.capabilities` is the input the runner consumes.
- [`docs/modules/security.md`](../modules/security.md) — when a network
  policy decision layer is added, sandbox is the enforcement layer
  below it.
