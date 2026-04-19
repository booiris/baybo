# Channel Plugin Redesign — Sidecar Model

## Status update (2026-04-19)

The transport prerequisites for this design have landed. The gateway now
runs a second listener — a Unix domain socket authenticated with
peer-cred + per-subprocess token (`x-aura-channel-token`) — and ships a
`ChannelSpawner` helper in `crates/gateway/src/spawn.rs` that mints a
token, pins it to the child pid in `ChannelTokenTable`, and hands the
socket path + token to the child via `AURA_CHANNEL_SOCKET` /
`AURA_CHANNEL_TOKEN` env vars. `ChildHandle::Drop` removes the token
from the table, so token lifetime is bounded by child lifetime. The
adjacent `admin` listener stays TCP+bearer for operator routes and
serves no channel functionality.

What's left is the plugin layer on top of that transport: the
`RemoteChannelProxy : ChannelAdapter`, a `PluginManager` that reads
manifests and registers proxies, and the `Supervisor` that shells out to
`ChannelSpawner::spawn`. Sections below that describe a WebSocket-based
RPC are superseded by "forward calls over the existing channel HTTP+SSE
surface" — plugins are already the canonical clients of that surface, so
adding a separate WebSocket channel is redundant. Treat this doc as
background motivation plus the manifest/CLI surface that is still valid;
the transport sections are historical.

## Problem

The current `ChannelAdapter` trait was designed for compile-time-linked
channels (TUI, HTTP). After the TUI refactor, only `HttpAdapter` remains as a
production `ChannelAdapter` implementation — the TUI is now a pure HTTP
client talking to the gateway. Future channels (Telegram, Discord, Slack,
Matrix, …) need to be **pluggable at runtime** so operators can install and
enable them without recompiling `aura-gateway`.

Constraints that shape the redesign:

1. **Platform SDKs are unavoidable.** Telegram, Discord, Slack each have a
   mature Rust (or other-language) SDK — `teloxide`, `serenity`, `slack-rs`.
   Any plugin model that makes these unusable is a non-starter because
   re-implementing the protocol by hand adds years of work and bugs.
2. **Crash isolation.** One misbehaving channel must not take down the whole
   gateway / agent.
3. **Hot lifecycle.** Install / enable / disable / restart without stopping
   the gateway.
4. **Secret discipline.** Bot tokens and OAuth credentials must land in the
   existing `SecretVault`, never in `env`/logs/process args.
5. **Cross-language option.** Allow a plugin author to pick Python for a
   platform whose best SDK is in Python (e.g. Matrix, Mastodon), without
   coupling to the Rust toolchain.
6. **Aura-native UX.** `aura channel install/enable/config/disable` should
   feel like `npm install` + `systemctl enable`, not like patching the
   gateway build.

## Current State

- `crates/channels/src/lib.rs` defines `ChannelAdapter` with an explicit
  (no-default) method set: `channel_type`, `start`, `send_response`,
  `send_stream_delta`, `send_notice`, `approval_gate`, `stop`.
- `ChannelRegistry` stores `Arc<dyn ChannelAdapter>` keyed by
  `ChannelType { Tui, Http }`.
- Only in-process `HttpAdapter` (`crates/gateway/src/http_adapter.rs`) is
  registered in production. `TuiAdapter` no longer implements the trait — it
  is a thin wrapper over `GatewayTransport` that speaks HTTP+SSE to the
  gateway.
- `ApprovalGateMap` is populated at registration time from each adapter's
  `approval_gate()`; `ToolExecutor` resolves per-call by
  `gate_map.get(user.channel)`.

No runtime plugin loading exists today; every channel must be compiled into
the binary.

## Plugin Model Options Considered

### A. Dynamic libraries (`cdylib` + `libloading`)
- Native perf; can share a `tokio::Runtime`.
- Rust has no stable ABI → plugin surface must be `extern "C"` +
  `#[repr(C)]`. `async_trait` does not cross FFI. `abi_stable` mitigates but
  imposes heavy constraints.
- Allocator mismatch → UB. Plugin panic crashes the host.
- Hot reload is risky (symbol lifetime, resident tokio tasks, SDK
  thread-locals).
- **Rejected.** SDK integration and crash isolation both fail.

### B. WASM Component Model (wasmtime + wit-bindgen)
- Strong sandbox, stable ABI, cross-language, trivial hot-reload.
- `teloxide` / `serenity` / `hyper` / `rustls` do not compile to
  `wasm32-wasip2`; socket support in WASI Preview 2 is still maturing.
- Plugin would have to re-implement each platform's protocol over raw HTTP.
- **Rejected.** Loses access to platform SDKs, which is the whole point.

### C. Out-of-process sidecar + RPC (**chosen**)
- Each channel = a normal binary (any language). SDKs used exactly like in
  a standalone bot.
- Crash isolation, independent deploy, independent versioning.
- Already aligned with gateway's existing HTTP+SSE surface — plugins become
  *clients* of the gateway rather than in-process *extensions*.
- Same model Anthropic's MCP uses for tool plugins (see
  `docs/todo/reintroduce-mcp-support.md` — the two designs should share as
  much manifest/supervisor machinery as possible).
- **Accepted.**

## Design: Sidecar Channel Plugins

### Architecture

```
┌───────────────────────────────────────────────────────────────────┐
│  aura-gateway (host process)                                      │
│                                                                   │
│  ┌─────────────────┐   ┌──────────────────┐   ┌───────────────┐   │
│  │ ChannelRegistry │←──│  PluginManager   │──→│  Supervisor   │   │
│  │  Arc<dyn        │   │  (load manifests │   │  (spawn +     │   │
│  │  ChannelAdapter>│   │   + register     │   │   watch +     │   │
│  │  per ChannelType│   │   RemoteProxies) │   │   restart)    │   │
│  └────────┬────────┘   └──────────────────┘   └───────┬───────┘   │
│           │                                            │           │
│      Router calls send_*                    managed sidecars only  │
│           │                                            │           │
│           ▼                                            ▼           │
│   RemoteChannelProxy ──── WS/HTTP ──┐         spawn() children    │
│                                      │                             │
└──────────────────────────────────────┼─────────────────────────────┘
                                       │
        ┌──────────────────────────────┼──────────────────────────────┐
        │                              │                              │
  ┌─────▼────────┐              ┌──────▼─────┐              ┌─────────▼────┐
  │ telegram     │              │ discord     │              │ slack         │
  │ sidecar      │              │ sidecar     │              │ sidecar       │
  │ (teloxide)   │              │ (serenity)  │              │ (slack-rs)    │
  └──────────────┘              └─────────────┘              └───────────────┘
```

Two ends speak a **documented RPC contract** (sketched below). Plugins
register themselves; the gateway installs a `RemoteChannelProxy` into the
`ChannelRegistry` that implements `ChannelAdapter` by forwarding each call
over the wire.

### Manifest

Every plugin ships a TOML manifest. Installing a plugin means placing the
binary + manifest under `~/.aura/channels/<name>/`.

```toml
# ~/.aura/channels/telegram/manifest.toml
name = "telegram"
version = "0.1.0"
api_version = "1"                      # Aura plugin protocol version
kind = "sidecar"                       # sidecar | self_hosted
exec = "aura-channel-telegram"         # on PATH, or absolute path
args = []
# Sidecar is launched with these env vars injected by the gateway:
#   AURA_GATEWAY_URL  = http://127.0.0.1:<gateway_port>
#   AURA_GATEWAY_TOKEN = <per-plugin bearer, minted into vault>
#   AURA_CHANNEL_NAME  = telegram
# plus anything under [env] below.
[env]
RUST_LOG = "info"

# Capabilities the plugin declares. The gateway mirrors these into the
# `RemoteChannelProxy` so `Router` / `Agent` can adapt output strategy.
capabilities = [
    "draft_updates",          # supports send_draft / update_draft / finalize_draft
    "reactions",              # add/remove_reaction
    "typing",                 # start/stop_typing
    "approval_inline",        # renders approvals natively (inline keyboard)
]

# Per-platform: what config/secrets the plugin needs. `aura channel config`
# and `aura channel enable` walk this schema to prompt the operator.
[config_schema.required.bot_token]
type = "secret"
description = "Telegram Bot API token from @BotFather"

[config_schema.optional.allow_user_ids]
type = "list<int>"
description = "allowlist; empty = everyone"

# Sandbox policy. See "Sandboxing" section for full semantics. Declared by
# the plugin author, reviewed by the operator on install/enable. If absent,
# the operator-wide default profile applies.
[sandbox]
profile = "standard"                       # relaxed | standard | strict
memory_max = "256M"
cpu_quota = "50%"
tasks_max = 64

[sandbox.network]
allow_hosts = ["api.telegram.org"]
allow_ports = [443]
allow_gateway = true                       # loopback to aura-gateway

[sandbox.filesystem]
readonly = ["/usr", "/etc/ssl", "/etc/resolv.conf"]
readwrite = ["{state_dir}"]                # expands to ~/.aura/channels/<name>/state

[sandbox.syscalls]
allow_groups = ["@system-service"]
deny_syscalls = ["ptrace", "bpf", "mount", "unshare"]

# Optional: integrity proof. `aura channel install` refuses a manifest/binary
# pair whose computed sha256 disagrees.
[integrity]
binary_sha256 = "…"
```

Schema fields to consider adding later: `min_aura_version`, `max_aura_version`,
`signed_by`, `homepage`.

### Install / Uninstall

```bash
aura channel install <source>
#   source ∈ { local path | https URL tarball | crates.io/<name> }
#   Steps:
#     1. Fetch binary + manifest.toml into ~/.aura/channels/<name>/
#     2. Verify manifest.api_version matches gateway's supported set
#     3. Verify integrity.binary_sha256 if present
#     4. Register entry in ~/.aura/channels/registry.json (installed, not enabled)
#     5. Do NOT launch anything
```

```bash
aura channel uninstall <name>
#     1. Require disabled state; refuse if running
#     2. Remove ~/.aura/channels/<name>/
#     3. Strip secrets under channels.<name>.* from vault (prompt: keep or purge)
#     4. Deregister from registry.json
```

### Configure

```bash
aura channel config telegram set bot_token
#   Reads manifest.config_schema, prompts for the value, writes to:
#     - SecretVault under channels.telegram.bot_token (for type=secret)
#     - workspace config under channels.telegram.* (for non-secret)

aura channel config telegram get
#   Prints non-secret config; redacts secrets to their placeholder IDs.

aura channel config telegram unset <key>
#   Removes the entry; enable will refuse if it was required.
```

Secrets live in `SecretVault` only. The plugin never sees raw secret values
in argv; they are injected into its env at launch, or fetched via a
`GET /v1/channels/{id}/config` call with the plugin's bearer token.

### Enable / Disable

```bash
aura channel enable telegram
#     1. Load manifest; require all config_schema.required keys satisfied
#     2. mint per-plugin bearer token → vault (channels.telegram.token)
#     3. workspace config: channels.telegram.enabled = true
#     4. Notify running gateway: POST /v1/admin/reload-channels
#        (or SIGHUP)
#     5. Gateway reconciles:
#        - sidecar: spawn child + supervise
#        - self_hosted: create RegistrationSlot in "awaiting" state
#     6. Block until register handshake succeeds (timeout 10s)
#     7. Output "✓ telegram running (channel_id=tg-<uuid>)"

aura channel disable telegram
#     1. Gateway POST /shutdown to the plugin's control endpoint,
#        or SIGTERM to managed child; SIGKILL after grace_period
#     2. Router unregisters RemoteChannelProxy
#     3. In-flight approvals keyed to this channel time out via existing
#        ChannelApprovalGate logic (fail-closed = Deny)
#     4. workspace config: channels.telegram.enabled = false
#     5. Output "✓ telegram disabled"
```

### Runtime Supervision (sidecar kind)

On gateway startup or reload-channels:

```
scan ~/.aura/channels/*/manifest.toml  (enabled only)

for each sidecar plugin:
    PluginManager::spawn(manifest):
        cmd = exec + args
        env:
            AURA_GATEWAY_URL      = http://127.0.0.1:<port>
            AURA_GATEWAY_TOKEN    = <minted bearer>
            AURA_CHANNEL_NAME     = <name>
            AURA_CHANNEL_SECRETS  = <JSON, decrypted in memory>
            (plus manifest.env)
        stdin  = null
        stdout = logs/channels/<name>.log
        stderr = logs/channels/<name>.log
        pid captured in PluginManager state

    Supervisor::watch(child):
        - on exit:
            if shutting_down: mark Stopped
            else: restart with exponential backoff
                   base 1s, factor 2, max 60s, max_retries 10
                   after max_retries → mark Failed + surface notice on TUI

for each self_hosted plugin:
    PluginManager::await_registration(manifest, deadline=∞)
    no spawn; RegistrationSlot visible in `aura channel list`
```

### Registration Handshake

```
Plugin → Gateway:
    POST /v1/channels/register
    Authorization: Bearer <AURA_GATEWAY_TOKEN from env>
    Body:
        {
            "name": "telegram",
            "version": "0.1.0",
            "api_version": "1",
            "pid": 12345,
            "capabilities": ["draft_updates", "typing", ...]
        }

Gateway → Plugin:
    200 OK
    {
            "channel_id": "tg-7c3e...",
            "outgoing_ws_url":  "ws://.../v1/channels/tg-7c3e.../outgoing",
            "incoming_post_url": "/v1/channels/tg-7c3e.../incoming",
            "approval_resolve_url": "/v1/approvals/{call_id}",
            "approval_stream_url": "/v1/approvals/stream?channel=tg-7c3e..."
    }
```

The bearer token is unique per plugin, scoped to that plugin's endpoints.
On successful register the gateway:

1. Constructs a `RemoteChannelProxy` bound to this `channel_id`.
2. Inserts it into `ChannelRegistry` under `ChannelType::Plugin("telegram")`.
3. Calls `proxy.approval_gate()` which returns a `RemoteApprovalGate` that
   pushes approval requests over the outgoing WS and awaits the resolve via
   existing `ApprovalQueue` wiring.
4. Emits a `channel.connected` event on the admin stream.

### RPC Surface (plugin ↔ gateway)

Two long-lived streams + two small REST endpoints:

- **Outgoing WS** (gateway → plugin, server push):
    ```
    { kind: "send_response",   session_id, content: [...] }
    { kind: "stream_delta",    session_id, delta: "..." }
    { kind: "notice",          session_id, level, text }
    { kind: "draft_start",     session_id, draft_id, content }
    { kind: "draft_update",    session_id, draft_id, content }
    { kind: "draft_finalize",  session_id, draft_id, content }
    { kind: "approval_request", call_id, session_id, tool, accesses, preview }
    { kind: "shutdown" }       # graceful
    ```
- **Incoming POST** (plugin → gateway, per event):
    ```
    POST /v1/channels/{id}/incoming
    { session_id, sender: {...}, content: [...], platform_metadata: {...} }
    ```
- **Approval resolve** (plugin → gateway):
    `POST /v1/approvals/{call_id}` (already exists).
- **Config fetch** (plugin → gateway, on boot):
    `GET /v1/channels/{id}/config` → plugin-specific secrets + options
    (alternative to injecting via env).

The "Outgoing WS" is the core: everything the gateway wants the plugin to
do flows over one ordered connection. Ordering is per `session_id`; the
plugin is free to multiplex its own side.

### ChannelType Evolution

```rust
enum ChannelType {
    Tui,                     // retained for back-compat; may retire
    Http,                    // gateway's own HttpAdapter (the TUI client)
    Plugin(PluginName),      // new; PluginName is newtype over String
}
```

Router logic does not change — `channels.get(channel)` still returns an
`Arc<dyn ChannelAdapter>`. For plugin channels, the adapter is
`RemoteChannelProxy` instead of an in-process impl.

`ApprovalGateMap` keyed by `ChannelType` works unchanged; each plugin's gate
is a `RemoteApprovalGate` that does the request/wait dance against the
plugin over WS.

### Capability Negotiation

Each `RemoteChannelProxy` carries a `CapabilitySet` struct populated from
`register`. Router/Agent consult this to pick output strategy:

```rust
struct CapabilitySet {
    draft_updates:             bool,
    multi_message_streaming:   bool,
    typing:                    bool,
    reactions:                 bool,
    approval_inline:           bool,
    // ...
}
```

- `draft_updates = true` → stream-delta-as-edit: send one draft, update on
  each delta, finalize on `send_response`. Matches how
  Telegram/Slack/Discord actually render a "growing" bot reply.
- `draft_updates = false && multi_message_streaming = true` → split at
  paragraph boundaries, send each as a new message.
- Both false → accumulate all deltas in gateway; send once on final
  `send_response`.

Strategy selection happens in a new `OutputStrategy` component sitting
between `Router` and `ChannelAdapter`; the current direct call
`adapter.send_stream_delta` is replaced by
`strategy.on_delta(adapter, ...)`.

### Approval Flow for Plugin Channels

Unchanged at the top: `ToolExecutor` calls `gate_map.get(channel).request`.
For a plugin channel:

```
RemoteApprovalGate::request(req):
    push PendingApproval{req, oneshot_tx} into plugin's ApprovalQueue
    send { kind: "approval_request", ... } over outgoing WS
    await oneshot_rx with timeout → Deny on timeout

plugin:
    receives approval_request
    renders platform-native UX (Telegram inline keyboard, Discord buttons,
    Slack action blocks, …)
    user clicks → plugin POSTs /v1/approvals/{call_id} { decision }
    gateway's existing handler resolves queue.resolve_by_call_id(...)
    which fires oneshot_tx → tool_executor unblocks
```

This reuses the entire existing `ApprovalQueue` + `/v1/approvals/*` machinery
with no modifications.

### Security Considerations

1. **Per-plugin bearer token**, minted into vault on `enable`, rotated on
   disable/re-enable. Scoped so it cannot call `/v1/sessions/*` or
   `/v1/admin/*` — only `/v1/channels/{own_id}/*` and `/v1/approvals/*`.
2. **Network binding**: gateway listens on loopback by default; plugins
   launched by the supervisor inherit this assumption. Remote plugins
   (self_hosted, not on localhost) are allowed only when operator opts in
   via `gateway.allow_remote_channels = true`.
3. **Secret delivery**: sidecar model injects decrypted secrets via env.
   Env is visible to the child only — not to other users under normal OS
   permissions. For additional hardening, offer pull model via
   `GET /v1/channels/{id}/config` so secrets never hit env.
4. **Plugin crash does not leak memory.** `RemoteChannelProxy` drops its WS
   on disconnect, unregisters from registry, pending approvals time out
   naturally.
5. **Manifest integrity**: `aura channel install` refuses a binary whose
   sha256 mismatches `manifest.integrity.binary_sha256`. Future: require a
   minisign/cosign signature from a configured set of publisher keys.
6. **Resource limits** (later): supervisor sets cgroup/RLIMIT on spawned
   sidecars. Not needed for v1 since operator chose to install each plugin
   explicitly.

### Sandboxing

Channel plugins are third-party code running in the operator's workspace.
Sidecar isolation lets us layer OS-level sandboxing on top of the already
existing process boundary — far cheaper than dylib or WASM would have been,
because plugins are just normal binaries.

#### Tiered model

Four strictness tiers. Operator picks a baseline (workspace config); a
plugin's `manifest.sandbox.profile` can stay at or below the baseline,
never above. Picking a stricter profile than the plugin declares is
always allowed (and fails loud if the plugin actually needs the denied
resource — prefer noisy failure over silent lockdown).

1. **Tier 0 — Process hygiene (default, zero deps).** Works on every
   platform; what the supervisor does unconditionally.
   - Run as the gateway's own user; no setuid descent.
   - `PR_SET_NO_NEW_PRIVS` / equivalent.
   - `stdin = /dev/null`, close inherited fds.
   - `RLIMIT_*` for CPU, RSS, `NOFILE`, `NPROC`, core size.
   - Kill on gateway exit (`PR_SET_PDEATHSIG` on Linux, `posix_spawn` +
     supervisor watchdog elsewhere).

2. **Tier 1 — Declarative host sandbox (Linux default when available).**
   Supervisor wraps `exec` through a host-provided sandbox. Two backends,
   picked by host capability, not by operator:
   - **`systemd-run --user --scope`** when the gateway runs under a user
     systemd instance. Encodes the manifest's sandbox policy into unit
     properties: `PrivateTmp`, `ProtectHome`, `ProtectSystem=strict`,
     `ReadWritePaths`, `RestrictAddressFamilies`, `IPAddressAllow/Deny`,
     `MemoryMax`, `TasksMax`, `SystemCallFilter`, `CapabilityBoundingSet=`.
   - **`bubblewrap` (bwrap)** otherwise. `--ro-bind` the read-only roots,
     `--bind` the writable state dir, `--unshare-all --share-net`,
     `--die-with-parent`, `--new-session`. Pair with a small seccomp
     filter (`--seccomp <fd>`) generated from the manifest's syscall
     policy. Filesystem restriction works out of the box; network
     restriction needs Tier 2 (netns + nftables) to be meaningful.

3. **Tier 2 — Namespaced isolation (Linux, opt-in).**
   Gateway constructs a per-plugin network namespace:
   - one veth pair, host side in a dedicated bridge;
   - `nftables` rules: allow outbound to manifest's `allow_hosts` (after
     DNS resolution at spawn time; IP refresh on re-connect);
     allow loopback to gateway port; deny all else;
   - a tiny stub DNS resolver in the host namespace so the plugin can
     resolve `allow_hosts` without leaking arbitrary DNS traffic.

   Paired with:
   - `seccomp-bpf` syscall allowlist (manifest `sandbox.syscalls`),
   - `landlock` filesystem allowlist (redundant with bwrap but covers
     cases where bwrap isn't present),
   - `cgroup v2` controllers for CPU / memory / PIDs / IO.

   Can be implemented in-process (supervisor calls `clone3` /
   `unshare(2)`) using the `nix`, `cap-std`, and `landlock` crates. Or
   delegated to `nsjail` / `firejail` — simpler to wire but adds a
   runtime dep.

4. **Tier 3 — Container / microVM (opt-in, heavy).** `podman` (rootless)
   for operators who want cross-platform uniformity, or `firecracker` /
   `crosvm` for true hardware isolation (multi-tenant or "untrusted
   upload" scenarios). Overkill for a personal-use channel sidecar;
   supported as an escape hatch, not the default.

macOS follows the same tier shape; backends differ:
- Tier 0 identical.
- Tier 1 uses `sandbox-exec` with a generated SBPL profile
  (deprecated by Apple but still functional). For homebrew-installed
  gateways this is the practical ceiling; deeper isolation requires
  App Sandbox which needs a signed bundle.

#### Policy compiler

A neutral `SandboxPolicy` struct is the single source of truth; each
host backend knows how to lower it. Pseudocode:

```
PluginManager::spawn(manifest):
    policy = SandboxPolicy::compile(
        manifest.sandbox,
        operator_baseline,
        platform,
    )
    backend = pick_backend(platform, policy.required_features)
    child = backend.spawn(
        exec   = manifest.exec,
        args   = manifest.args,
        env    = compose_env(...),
        policy = policy,
    )
    Supervisor::watch(child)
```

`pick_backend` prefers the strongest backend whose `required_features`
the host supports. Operator can pin a backend in workspace config, e.g.
`channels.sandbox.backend = "bwrap"` or `"systemd"`.

If the host cannot satisfy `policy.minimum_tier` (say the operator
requires Tier 2 but the host has no `unshare`/`landlock`), `aura
channel enable` refuses to launch.

#### UX: review at install and enable

Never auto-grant. Whenever the declared sandbox policy widens what the
operator already has on disk (new host, new writable path, new syscall
group), `aura channel install` and `aura channel enable` display a diff
and require explicit confirmation.

```
$ aura channel install ./aura-channel-telegram-0.1.0.tar.gz
Installing telegram 0.1.0 (sha256 OK).

Declared sandbox (profile: standard):
  Network:  api.telegram.org:443, loopback (gateway)
  Disk:     read  /usr /etc/ssl /etc/resolv.conf
            write ~/.aura/channels/telegram/state
  Memory:   256M cap
  Syscalls: @system-service minus ptrace,bpf,mount,unshare

Proceed? [y/N]
```

`--trust` flag skips the prompt (CI, scripts). `--profile strict`
overrides downward. `--profile relaxed` rejected unless operator also
passes `--trust-relaxed`; `profile = "relaxed"` effectively means "no
sandbox" and should feel scary.

#### What the plugin author has to do

Almost nothing.

- Code does not change. The plugin opens its platform SDK as usual.
  `teloxide` / `serenity` / `slack-rs` see a normal OS; they just can't
  reach anything outside the declared allowlist.
- Declare honestly in the manifest: hosts, ports, paths, syscall groups.
  Missing a host → plugin startup errors on first outbound call →
  operator adds it and reinstalls. Loud failure is intended.
- Optional: for belt-and-suspenders, plugin may drop further privileges
  from inside using the `extrasafe` or `birdcage` crates after it has
  opened its listening socket / initial connections. Treated as a plugin
  hygiene detail, not required.

#### Interaction with the registration token

The per-plugin bearer token is scoped to `/v1/channels/{own_id}/*` +
`/v1/approvals/*`. Sandbox network policy narrows *which hosts* the
plugin can dial; token scope narrows *what it can do* once it dials
loopback. Both layers stack — an exfiltration attempt has to pierce
host firewall and present a valid token scoped to a different endpoint.

#### Non-goals for v1

- Fine-grained per-session capability gating (e.g. "this plugin instance
  can only touch sessions created through it"). Hard to enforce without
  changing the session-id contract; punted.
- Signed manifests / publisher key trust store. Stub-compatible (the
  `signed_by` field is reserved) but implementation waits for demand.
- Live policy tightening (reducing a running plugin's privileges mid-
  session). Policy change requires restart.

### CLI Surface

```
aura channel list                        # installed + enabled + state
aura channel list --json                 # structured (for scripts)
aura channel install  <source>
aura channel uninstall <name>
aura channel enable  <name>
aura channel disable <name>
aura channel config  <name> [get|set|unset] [key [=value]]
aura channel logs    <name> [-f] [--lines N]
aura channel restart <name>
aura channel doctor  <name>              # health check: config, reachability, register
aura channel info    <name>              # manifest + capability + current channel_id
```

All commands route through the gateway's admin API so they work whether
invoked from the same machine or a remote operator with a gateway token.

### Agent / Skill Side Visibility

The Agent loop itself remains channel-agnostic. Messages arrive with
`message.channel = ChannelType::Plugin("telegram")`; outgoing paths use the
same key in `send_to_channel`. Three places need to see capability
information:

1. **SlashHandler** — different channels expose different slash commands.
   E.g. `/dashboard` is TUI-only. Each handler can consult the source
   channel's capability set to filter.
2. **OutputStrategy** — as above, picks draft vs. one-shot vs. multi-message.
3. **Approval prompt formatting** — with `approval_inline`, send a
   structured prompt; without, send text + ask user to reply with a
   command.

### State Machine (per plugin)

```
     uninstalled
          │ install
          ▼
     installed ─────────── uninstall ─────────► uninstalled
          │ enable                                    ▲
          ▼                                           │
       enabled (desired=running)                      │
          │                                           │
          │ gateway spawn                            disable (idempotent)
          ▼                                           │
     launching ─── register ──► running ───────► stopping ──► stopped
          │ spawn error             │ crash        ▲                │
          │                         ▼              │                │
          └──► failed ◄─── max_retries             └────────────────┘
```

`failed` requires explicit `aura channel restart` to transition back to
`launching`.

### `ChannelAdapter` Trait Revisions

The current minimal trait stays, but grows to accommodate plugin UX:

- `send_draft(&self, session_id, draft_id, content)`
- `update_draft(&self, session_id, draft_id, content)`
- `finalize_draft(&self, session_id, draft_id, content)`
- `cancel_draft(&self, session_id, draft_id)`
- `start_typing(&self, session_id)`
- `stop_typing(&self, session_id)`
- `add_reaction(&self, session_id, message_id, emoji)`
- `remove_reaction(&self, session_id, message_id, emoji)`
- `healthy(&self) -> bool`

**Open question**: flat trait vs. sub-traits (`DraftChannel`,
`ReactiveChannel`). Leaning toward sub-traits so `ChannelAdapter` stays
small and `HttpAdapter` does not have to implement methods it has no
surface for. Routers would `downcast_ref::<dyn DraftChannel>()` guided by
the capability set.

## Migration / Phased Plan

**Phase 0 — prep** (no user-visible change)
- Split `ChannelType` into an enum-with-payload allowing `Plugin(String)`
  behind a feature flag.
- Introduce `RemoteChannelProxy` as a stub against a mock plugin in tests
  (echo channel). Run the full agent test suite through it.
- Freeze the WS/HTTP RPC surface in a spec doc
  (`docs/modules/channel-plugin-protocol.md`).

**Phase 1 — plumbing**
- `PluginManager`, `Supervisor`, manifest loader.
- `/v1/channels/register` endpoint + per-plugin bearer token scope.
- `aura channel install/enable/disable/list` CLI, no draft/capability UX
  yet.
- Reference plugin: `aura-channel-echo` (a dev-only plugin that echoes
  input; lives in `crates/channel-echo/` built as a separate binary).

**Phase 2 — capability + draft**
- Capability negotiation, `OutputStrategy` component, draft lifecycle on
  the WS.
- Sub-trait split (or flat additions — decide based on Phase 0 ergonomics).

**Phase 3 — first real plugin**
- `aura-channel-telegram` as a separate repo/binary using `teloxide`.
- Validates the protocol end-to-end, drives any remaining schema fixes.

**Phase 4 — polish**
- `aura channel doctor`, `aura channel logs`, health-check gossip on
  `/v1/status`.
- Optional: signed manifests, resource limits on supervisor.

## Open Questions

1. **Protocol encoding**: JSON over WS is easiest; consider
   MessagePack/CBOR if payload size matters. gRPC considered and rejected
   for v1 (tonic is heavy for plugin authors in other languages).
2. **Flat vs. sub-traits** for draft/reactive methods — decide during
   Phase 2.
3. **Plugin-to-plugin messaging** (e.g. bridge Telegram ↔ Discord) is out
   of scope; if needed later, add a `/v1/bridges` layer that subscribes
   to multiple plugin outgoing streams.
4. **Session ID mapping**: each plugin needs a stable mapping between
   platform IDs (Telegram `chat_id`, Discord `channel_id.guild_id`) and
   Aura session ids. Should be plugin-local (stored in a sqlite the plugin
   owns) or gateway-central (stored in Aura's libsql, exposed via
   `/v1/channels/{id}/sessions`). Leaning plugin-local to keep the plugin
   self-contained.
5. **Cross-version compatibility**: manifest carries `api_version`, but the
   negotiation protocol (which fields are tolerated/ignored) needs a
   written policy.

## Related

- `docs/modules/channels.md` — current trait/registry design; to be rewritten
  after this lands.
