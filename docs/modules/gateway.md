# gateway - HTTP Server and Service Installer

## Overview

`aura-gateway` is Aura's headless backend. It runs **two listeners** side
by side against the same manager graph:

1. **Admin listener** — TCP, bearer-token authenticated. Surfaces the
   operator controls (config, jobs, cron, memory, traces, skills, tools,
   channels-list, llm, status) that mirror the CLI command families. No
   chat content or session data flows here.
2. **Channel listener** — loopback TCP (`127.0.0.1:<ephemeral>`),
   authenticated by either the TUI pre-shared key or a per-subprocess
   token. The chosen port is published to `<workspace>/channel.port`
   (mode `0o600`) so the TUI and spawned sidecars discover it without
   a config roundtrip. The listener hosts a single
   `GET /v1/channel-ws` endpoint that upgrades authed requests to a
   WebSocket. Each connection registers itself as a live
   [`aura_channels::Channel`] on the workspace [`ChannelRegistry`] and
   exchanges MessagePack-framed events with the agent. The built-in
   TUI and every out-of-process sidecar plugin speak this one
   protocol. `127.0.0.1` binding is hardcoded — config cannot loosen
   it to `0.0.0.0`.

Per-connection state lives in `src/channel/adapter.rs::Sidecar`: it
builds the [`aura_channels::Channel`] handle the registry sees, owns an
outbound frame mpsc, and spawns one pump task that drains the receiver
onto the WS sink. The agent's [`AgentOutput`] stream, the
[`ApprovalGate`] waker, and the inbound loop's `resolve_approval` path
all push [`Frame`](aura_channels::wire::Frame)s through the same
mpsc, so the pump is the single serialisation point onto the wire.

The gateway is driven by the `aura gateway …` command tree. `start` runs
both listeners in the foreground; `install` / `enable` / `disable` /
`uninstall` / `status` manage the platform service unit; `token
{show|rotate}` manages the admin bearer token. The binary entrypoint
intercepts `Commands::Gateway` in `src/main.rs` before the CLI dispatcher
and routes it to `src/gateway_cmd.rs` — same pattern as `Commands::Tui`.

Configuration lives in `aura_config::GatewayConfig` (a new section on
`AuraConfig`). The stub `HttpChannelConfig` at
`crates/config/src/channels.rs:60` is kept for one release for backwards
compatibility but is no longer read; the gateway owns its own settings.

## Design Decisions

### Two listeners, one router graph

Splitting the gateway isolates blast radius: a leaked admin bearer
token cannot read chat content or message sessions, and a sidecar
channel plugin running as a child of the gateway has no admin surface
to hit even if compromised. Both listeners share the same manager
graph (`SessionManager`, `JobManager`, …); the channel listener hosts
the `GET /v1/channel-ws` upgrade path and nothing else, so the router
sees a single `IncomingMessage` stream regardless of whether a given
frame came from the built-in TUI or an out-of-process sidecar. Each
accepted WS connection registers an [`aura_channels::Channel`] against
the workspace registry, which the router then dispatches to by
`ChannelType` — no parallel agent pipeline.

### Admin token, stored in `SecretVault`

On `aura gateway enable`, `GatewayToken::mint_if_absent` either reads
the current token or generates a fresh 32-byte random value (hex-
encoded) and writes it under the vault key `gateway.admin_token` — the
same AES-256-GCM store the rest of Aura uses. Vault loads also accept
the legacy `gateway.auth_token` key for backwards compatibility with
installs that pre-date the listener split. `token show` reads,
`token rotate` overwrites. `uninstall` removes the service unit but
leaves the token in the vault; `token rotate` is the explicit way to
invalidate a leaked token. Because the vault requires the master
encryption key, the token's confidentiality rides on the same root
secret as every other credential in the project.

### Channel auth — vault-issued tokens (TUI + subprocess)

The channel listener enforces a single header on every request:
`x-aura-channel-token: <hex>`, looked up against the in-memory
`ChannelTokenTable`. Each entry carries a `ClientIdentity { pid, label }`
and the auth middleware uses the reserved label
`aura_gateway::TUI_CLIENT_LABEL` ("tui") to distinguish a bundled-TUI
connection from a subprocess sidecar — the same channel-token pipeline
carries both.

Two flavours of token end up in the table:

- **TUI token.** Generated on every `aura gateway start`, written to
  the secret vault under the key
  `aura_gateway::TUI_TOKEN_VAULT_KEY` ("gateway.tui_token"), and
  registered with the gateway's own pid + the reserved TUI label. The
  gateway holds the returned `TokenHandle` for the entire lifetime of
  `start`, so the in-memory entry is revoked the moment shutdown
  drops it. The vault row stays around between starts (the next start
  overwrites it), but a TUI presenting a vault value from a previous
  generation is rejected because the in-memory table has the new
  value.
- **Subprocess capability tokens.** Minted inside
  `ChannelSpawner::spawn` before `Command::spawn`, handed to the
  child via the `AURA_CHANNEL_TOKEN` env var, and revoked when the
  owning `ChildHandle` drops. The PID stored on the identity is the
  child PID and is retained for diagnostics (log lines,
  `/v1/status`) — it is *not* part of the auth check, since loopback
  TCP carries no kernel-attested peer credential and the token's
  uniqueness + same-UID-only delivery already cover the threat
  model.

For runtimes whose WebSocket client cannot set custom headers (any
WHATWG `WebSocket`, browser-style clients) the listener also accepts
`?token=<hex>` on the upgrade URL. The auth middleware strips the
query parameter from the request URI before the inner `TraceLayer`
runs so the secret never lands in structured logs.

#### Threat model

The vault-token scheme is a **workspace-binding** credential, *not*
a defence against a same-UID hostile process.

*Designed to reject*
- connections from a different Unix user — the libsql vault file is
  `0o600` and `<workspace>/channel.port` is also `0o600`, so another
  UID can't even locate the listener's port, let alone read the
  freshly-rotated TUI token;
- cross-workspace mix-ups — every workspace has its own libsql
  database with its own (master-key-encrypted) `gateway.tui_token`
  row, so a TUI pointed at workspace B will never come up with the
  matching value for workspace A;
- stale reconnects after a gateway restart — the in-memory
  `ChannelTokenTable` is rebuilt empty on every start, so a TUI
  still holding the previous generation's token must re-read the
  vault.

*Not designed to defend against*
- a malicious process running as the same UID as the gateway. Such a
  process can read the libsql file, recompute the master key (or
  read the env var that holds it), decrypt the row, and present the
  same token. Treat "same-UID local adversary" as out of scope for
  this mechanism — the practical tools are file permissions on the
  vault + master-key file, service-manager sandboxing, and OS-level
  isolation (seccomp, sandbox-exec, etc.).

The same caveat applies to subprocess tokens: a same-UID process can
read `/proc/<pid>/environ` on Linux and lift the env-var-delivered
token. They are workspace/lifetime binding, not hostile-process
resistance.

All token compares are constant-time via
`aura_gateway::constant_time_eq` (re-exported from
`crate::auth::token` and used internally by `ChannelTokenTable::lookup`'s
underlying `DashMap` plus the header-extraction path). `/healthz` and
`/readyz` skip auth on both listeners.

### Admin auth — bearer token, URI sanitisation before tracing

`auth::admin::require_admin_token` accepts `Authorization: Bearer
<token>` first and falls back to `?token=<token>` for embedded links
(SSE streams opened from a browser, quick-copy `curl`s). Comparisons
run through `constant_time_eq`. After a successful compare the
middleware strips `?token=` from the request URI in place so
`tower_http::trace::TraceLayer` never sees the token in structured
logs.

### `LeakDetector` rule is attached at construction, not at mint

`LeakDetector::add_rule` takes `&mut self`; the detector is shared as
`Arc<LeakDetector>` across every manager once the runtime is wired, so
there is no way to register a new redaction rule after construction.
`gateway_cmd::start` therefore opens the vault once *before* tracing
init: it reads or auto-mints the admin token, generates the fresh TUI
token + writes it to `gateway.tui_token`, and only then calls
`runtime::build_leak_detector` with both values registered as
`LeakAction::Replace` rules. The same `Arc<LeakDetector>` is then
forwarded into `build_managers`, so the runtime graph's
`SecurityGateway` redacts both credentials on every log surface.

### Sidecar owns the outbound pump

Each accepted WS upgrade builds one `Sidecar`
(`src/channel/adapter.rs`). The struct owns a
`mpsc::Sender<Frame>` (the outbound frame mpsc) plus a translator task
that converts `AgentOutput` → `Frame` and forwards it onto the same
mpsc. `Sidecar::build` returns the `Arc<Channel>` the route task hands
to `ChannelRegistry::register`; the registry then populates the
shared `ApprovalGateMap` from `Channel::approval_gate()` so tool
approvals resolve to this connection. A single pump task drains the
receiver and writes each frame to the WS sink with
[`rmp_serde::to_vec_named`](aura_channels::wire::encode) —
everything fans *in* to the mpsc, the pump is the only thing that
touches the socket. When the registry `unregister`s on disconnect the
gate map eviction and `Sidecar::into_pump()` drop the last
`frame_tx` clones, so the pump exits cleanly without a separate stop
signal.

### Tool approval over the sidecar WS

`Channel::approval_gate()` returns a `ChannelApprovalGate` backed by a
per-connection `ApprovalQueue`. When a tool call hits the gate the
entry is pushed on the queue and the waker closure sends an
`ApprovalRequested` frame through the outbound mpsc. The client echoes
a `ResolveApproval { call_id, decision }` frame; the inbound loop
calls `Sidecar::resolve_approval`, which pops the matching entry off
the queue and sends an `ApprovalResolved` frame back through the same
mpsc so the client can drop any optimistic UI. The 5-minute timeout
mirrors the TUI's original budget — long enough for a human, short
enough that forgotten prompts don't pin tool executors forever.

Approvals are *per-connection*, not per-session: a leaked or
disconnected sidecar simply evicts its gate, and `ToolExecutor` falls
back to the registry-wide fail-closed `AutoDenyGate` for that
`ChannelType` until a fresh connection registers.

### Gateway owns its own DTOs — utoipa stays in the gateway

Route handlers call the manager `pub async fn` methods directly
(`SessionManager`, `JobManager`, `CronScheduler`, `MemoryManager`,
`TraceStore`, `SkillRegistry`, `ToolRegistry`) and serialise into DTOs
defined in `crates/gateway/src/api/dto.rs`. CLI handler output is not
reused — those are built around `CommandContext` + `OutputFormat` and
would force a cross-crate refactor of every `crates/cli/src/commands/`
module. The duplicated surface is small and the independence is worth
more than the line count saved.

`api/dto.rs` is also the **only** place in the workspace that depends
on `utoipa`. Domain crates (`aura-model`, `aura-job`, `aura-cron`,
`aura-tools`) stay HTTP-framework-agnostic — no `#[derive(ToSchema)]`,
no `#[schema(...)]` attributes leaking into them. The gateway defines
a mirror type for every domain type that appears on the wire and
provides `From<Domain>` conversions; handlers build DTOs at the seam
via `.map(DomainDto::from)`. Mirror types keep their bare name
(`Job`, `CronJob`, `MemoryEntry`, `ChannelType`, …) so the generated
OpenAPI schemas — and the TypeScript types downstream — are stable
across the refactor. Adding a field to a domain type now requires an
explicit edit in `dto.rs` to surface it on HTTP; that's a feature, not
a bug, because the drift test below will force the question.

A small exception: `MemoryCategory` is an adjacent-tagged unit-variant
enum (`#[serde(tag = "type", content = "value")]`), which the utoipa
derive macro currently can't generate. The mirror has a hand-rolled
`PartialSchema + ToSchema` impl inline in `dto.rs`. Any similar enum
shape added later should follow the same pattern rather than
regressing the domain crate to depend on utoipa.

### OpenAPI spec generation and the TypeScript client

`admin/mod.rs::v1_router_and_spec()` returns `(Router<AdminState>,
OpenApi)` in one call: the submodules each expose `OpenApiRouter`s
carrying their `#[utoipa::path]`-annotated handlers, `AdminApiDoc`
seeds the `info` + `tags` block, and `split_for_parts()` emits both
the axum `Router` and the finished `OpenApi` document. `server.rs`
wires state and auth; `api/openapi.rs` mounts `GET /v1/openapi.json`
as a last step so the same document the test below writes to disk is
also served live.

The document is checked in at `docs/openapi.json` and kept honest by
`crates/gateway/tests/openapi_spec_sync.rs`: the test regenerates the
spec from `v1_router_and_spec()` and compares byte-for-byte. On
intentional surface changes, run with `UPDATE_OPENAPI=1 cargo test -p
aura-gateway --test openapi_spec_sync` to rewrite the snapshot. A
drifted spec fails CI, which guarantees the frontend can regenerate
types without hitting a stale file.

The frontend consumes the spec through two tools:

The `/v1/logs` surface has two endpoints:

- `GET /v1/logs` — paged snapshot from `LogBuffer::query` (filter by
  `level`, `q`, `since`, `until`, `limit`, `offset`; `total` is
  independent of pagination).
- `GET /v1/logs/stream` — SSE stream subscribed to the same buffer's
  `broadcast::Sender`. Each captured record that matches the query
  params ships as an `event: log` frame with `LogEntry` JSON as `data`.
  Back-pressure is bounded: a client that falls behind receives an
  `event: lagged` with the drop count, then resumes. The admin auth
  middleware accepts `?token=…` as a fallback because `EventSource`
  can't set headers; the webui's Live toggle uses it for real-time
  tail without polling.

- `openapi-typescript` (`npm run gen:api`, also wired into `npm run
  build`) reads `docs/openapi.json` and writes
  `web/src/api/schema.d.ts` — a pure `.d.ts` with `paths` and
  `components` maps. No runtime code is emitted.
- `openapi-fetch` consumes that schema type at runtime. The thin
  wrapper in `web/src/api/client.ts` (`createAdminClient({ baseUrl,
  token })`) returns a typed `Client<paths>` with Bearer auth
  pre-applied. Every admin call the UI makes therefore has request
  paths, params, body shape, and response variants checked by `tsc`
  against whatever the Rust router actually exposes.

This gives a two-step drift alarm: (1) Rust test fails if the spec
drifted from the router; (2) `tsc` fails in the web build if
`schema.d.ts` was regenerated but a caller still uses the old shape.
The web bundle is unauthenticated by design (see "WebUI" below), so
the generated client is only a convenience for the operator's
browser — `/v1/*` still enforces `require_admin_token`.

### WebUI — embedded React dashboard, no `rust-embed`

The admin TCP listener doubles as a web frontend. Sources live at the
repo root in `web/` (React 19 + TypeScript + Vite + Tailwind v4 +
react-router, neo-brutalist style). `crates/gateway/build.rs` walks
the relevant `web/` source inputs (`src/`, `index.html`,
`package.json`, `tsconfig*.json`, `vite.config.ts`,
`docs/openapi.json`, and the pnpm lock/workspace files). If those
inputs changed since the last successful web build, it runs
`pnpm --filter aura-web build`; otherwise it reuses the existing
`web/dist/`. It then zstd-compresses each emitted asset and writes
`$OUT_DIR/webui_assets.rs` with a static asset table pairing the path,
pre-computed MIME string, and compressed bytes (`html`, `js`, `css`,
`svg`, `png`, `ico`, fonts, fallbacks). No `rust-embed`, no
`mime_guess`; `api::webui` lazily decompresses the table into memory on
the first request and then serves the cached bytes afterward.

`api::webui::serve` is mounted via `Router::fallback` so `/`,
`/assets/...`, and any unmatched path resolve to a baked asset while
`/healthz`, `/readyz`, and `/v1/*` keep their explicit handlers.
Unknown non-asset paths fall back to `index.html` so SPA deep links keep
working if we ever move off `HashRouter`. Requests under `/assets/`
that miss return **404** instead of SPA-falling-back — this prevents a
stale `<script src="/assets/index-OLDHASH.js">` on a cached
`index.html` from being served as `text/html` and tripping the browser's
strict-MIME guard (which manifests as a blank page after a rebuild).
`index.html` is sent with `Cache-Control: no-cache` so bundle-hash
changes take effect on the next load; hashed `/assets/*` carry
`public, max-age=31536000, immutable`.

The bundle is unauthenticated on purpose: the embedded HTML/JS carries
no server-side capability, and every privileged data path still goes
through `/v1/*` behind `require_admin_token`. Treating the webui as a
static inert asset rather than a privileged surface keeps the bearer-
token contract simple — tokens gate data, not pages.

Admin API calls from the web bundle go through the typed client at
`web/src/api/client.ts`, which wraps `openapi-fetch` with a
`Client<paths>` derived from the checked-in OpenAPI document. See the
"OpenAPI spec generation and the TypeScript client" design note above
for the regeneration flow.

Release flow:

```bash
pnpm install
pnpm --filter aura-web build
cargo build --release -p aura-gateway
```

If the tracked web inputs changed, `build.rs` attempts
`pnpm --filter aura-web build` automatically during `cargo build`. If
that build fails or the frontend toolchain isn't available,
`build.rs` falls back to the existing `web/dist/`; if no dist exists at
all, it writes a one-line placeholder `index.html` so backend-only
development still compiles.

**Dev flow (HMR)**: rebuilding the gateway on every frontend tweak is
slow, so for UI iteration run the two sides separately:

```bash
# terminal 1 — debug gateway, admin listener on 127.0.0.1:8889
cargo run -- gateway start

# terminal 2 — Vite dev server with HMR on http://localhost:5173
cd web && npm run dev
```

`web/vite.config.ts` proxies `/v1`, `/healthz`, and `/readyz` to
`127.0.0.1:8889`, so the browser only talks to the Vite origin. This
avoids the CORS path entirely — `AdminAuthProvider` keeps `baseUrl =
window.location.origin` (the Vite origin), and bearer-token auth
still works end-to-end because the proxy forwards the `Authorization`
header. If you need to point the web bundle at a gateway on a
different origin (e.g. running `npm run dev` against a remote gateway),
add that origin to `gateway.cors_allowed_origins` in `aura.json` and
use the LoginScreen's **Advanced → Gateway base URL** field to
override the stored `baseUrl`.

### Platform installers behind Cargo features

```
default      = ["linux", "macos"]
linux        = []              # systemd installer — renders ~/.config/systemd/user/aura-gateway.service
macos        = []              # launchd installer — renders ~/Library/LaunchAgents/com.aura.gateway.plist
test-support = ["dep:tempfile"] # cross-crate test helpers (CLAUDE.md gating rule)
```

One feature per OS. Any future OS-specific gateway code (beyond the
service installers) should reuse the same flag rather than introduce a
narrower one — builds stay legible with a single knob per platform.

The factory `installer::for_current_platform(user_mode)` returns the
matching impl or `InstallerError::Unsupported(os)` when no installer is
compiled in. Every installer implements the same trait:

```rust
trait ServiceInstaller {
    fn unit_path(&self) -> PathBuf;
    fn render_unit(&self, ctx: &InstallContext) -> String;
    fn install(&self, ctx: &InstallContext) -> Result<PathBuf>;
    fn enable(&self) -> Result<()>;
    fn disable(&self) -> Result<()>;
    fn uninstall(&self) -> Result<()>;
    fn status(&self) -> Result<ServiceStatus>;
}
```

`render_unit` is deliberately separate from `install` so tests can
snapshot the rendered output without writing to disk.

### `ExecStart` resolution is explicit, not implicit

`installer::resolve_exec_start(explicit)` follows a strict precedence:
`--exec-start` flag → `which aura` on `$PATH` → `std::env::current_exe()`.
Under `cfg(debug_assertions)` with no flag the resolver **refuses** with
a clear hint: `target/debug/aura` disappears after `cargo clean`, and
an installed service pointing at it would break silently. This is a
real footgun from early versions and the refusal is intentional.

`AURA_CONFIG_PATH` is captured at install time and embedded as
`Environment=AURA_CONFIG_PATH=...` in the systemd unit (or as an entry
under `EnvironmentVariables` in the launchd plist). systemd does not
inherit the invoking shell's env, so the capture is load-bearing.

### Service unit hardening — no restart-loop footgun

Both unit files include a small restart delay (`RestartSec=2s` on
systemd, `ThrottleInterval=2` on launchd). The per-workspace singleton
lock (`src/singleton.rs`) will reject a second `start` invocation that
fires before the previous process has unwound; without the delay a
restart loop can thrash the lock. The systemd unit also sets
`TimeoutStopSec=30s` to match `GatewayConfig::shutdown_grace_secs`.

## CLI Surface

All commands live under `aura gateway` and are also usable over slash
mode once a channel exposes them (not wired yet; the `Gateway` arm
returns `UnknownCommand` from the normal dispatcher because
`src/main.rs` intercepts it before dispatch).

| Subcommand                                | Effect                                                                                               | Mutating |
| ----------------------------------------- | ---------------------------------------------------------------------------------------------------- | -------- |
| `start`                                   | Run the server in the foreground. Prints `http://<bind>/v1/status?token=<TOKEN>` quick URL.           | —        |
| `install [--system] [--exec-start <p>]`   | Write the platform service unit. `--system` flips from user mode to root/system-wide.                 | yes      |
| `enable`                                  | Mint the auth token if absent; best-effort enable the service for autostart.                          | yes      |
| `disable`                                 | Mark the service as not autostarting at boot.                                                         | yes      |
| `uninstall [--yes]`                       | Remove the service unit. The vault token is left in place; use `token rotate` to invalidate a leaked one. | yes      |
| `status`                                  | Report `NotInstalled` / `Installed` / `Enabled` / `Running` / `Unknown`.                              | —        |
| `token show`                              | Print the current token. Errors if none minted yet.                                                   | —        |
| `token rotate [--yes]`                    | Overwrite the token with a fresh 32-byte value.                                                       | yes      |

The handler lives in `src/gateway_cmd.rs` and uses the `boot::` helpers
to build a throwaway `SecretVault` when a subcommand needs it. `start`
is currently a stub that returns an informative error pointing at
`docs/modules/gateway.md` — the long-lived server boot is pending the
runtime extraction tracked in the follow-up todo
(`docs/todo/gateway-runtime-extraction.md`).

## HTTP API

Routes are split between the two listeners. `/healthz` and `/readyz`
live on both; every other authenticated route goes through the
listener-specific auth middleware.

**Admin listener (TCP, bearer token)** — `auth::admin::require_admin_token`:

```
GET    /healthz                         liveness (no auth)
GET    /readyz                          managers ready (no auth)
GET    /v1/status                       aura status mirror
GET    /v1/config                       redacted config snapshot (read-only)
PUT    /v1/config                       { path, value } → 200 { path, written_to, requires_restart }
DELETE /v1/config                       { path } → 200 { path, written_to, requires_restart }

GET    /v1/jobs                         ?status=pending|in_progress|completed|failed|stuck
GET    /v1/jobs/:id
POST   /v1/jobs/:id/cancel

GET    /v1/cron
POST   /v1/cron                         { schedule, text, name? }
GET    /v1/cron/:id
DELETE /v1/cron/:id

GET    /v1/memory                       ?category=&limit=
POST   /v1/memory                       { content, category }
DELETE /v1/memory/:id

GET    /v1/traces/:session_id

GET    /v1/skills
GET    /v1/tools
GET    /v1/channels                     read-only registry list
GET    /v1/llm

GET    /v1/openapi.json                 live OpenAPI 3.1 document for the admin surface
```

**Channel listener (loopback TCP, vault-issued tokens)** —
`auth::channel::require_channel_auth`:

```
GET    /healthz                         liveness (no auth)
GET    /readyz                          managers ready (no auth)

GET    /v1/channel-ws                   WebSocket upgrade (MessagePack frames)
```

Hitting a channel route on the admin listener returns `404` — the route
is not mounted there at all, so a leaked admin token yields nothing. The
`admin_has_no_channels` integration test enforces this.

The WS protocol is defined by [`aura_channels::wire::Frame`]
(tagged on `kind`, MessagePack-named). The client opens with a
`Register { token, channel_type, protocol_version }` frame; the server
validates it via `channel::handshake::validate_register` against the
[`auth::channel::AuthedClient`] the middleware already attached (TUI
token → must claim `"tui"` and a non-empty `session_id`; subprocess
token → must match the minted identity's `(pid, label)` and claim a
non-reserved channel type).
`RegisterAck { ok, reason? }` closes the handshake; subsequent frames
are `Message` (user input in, final assistant response out), `Delta`
(streaming assistant text, server → client), `Notice` (out-of-band
warn/error, server → client), `ApprovalRequested` / `ApprovalResolved`
(server → client), and `ResolveApproval` (client → server). Session
ids are client-generated UUIDs: the router resolves or creates the
session on first message via `SessionManager::get_or_create`, so no
client has to pre-provision one.

For session-scoped TUI clients only, two additional frames carry the
persistent input-history ring: `HistorySnapshot { session_id, entries }`
is pushed once by the server immediately after a successful
`RegisterAck` (sidecars never see it), and `HistoryAppend { session_id,
entry }` is sent by the client after every accepted submission. The
gateway owns the encrypted ring via `channel::TuiHistoryStore` (vault
key `aura.tui.input_history`, 500-entry cap, consecutive-duplicate
dedup); concurrent appends from multiple TUIs on the same gateway
serialise through a `tokio::sync::Mutex` on the store. TUI clients
never open the vault themselves. See [`tui.md`](./tui.md) and
[`security.md`](./security.md).

Mutation endpoints (`PUT /v1/config`, `DELETE /v1/config`) write
through to the same on-disk `aura.json` that `aura config set/unset`
targets. The in-memory `Arc<AuraConfig>` held by managers is **not**
swapped; `requires_restart: true` in the response signals that the
gateway must be restarted for the change to take effect. If the
gateway was booted without a config path (pure-default boot), both
endpoints return `400 Bad Request`.

## Runtime Assembly

`start` builds the full manager graph — `SessionManager`,
`JobManager`, `CronScheduler`, `MemoryManager`, `TraceStore`,
`SecurityGateway`, `SkillRegistry`, `ToolRegistry`, `ToolExecutor`,
`SkillAssessor`, `LlmClient`, `WorkspaceManager`, `LeakDetector`,
`ChannelRegistry`, `CostTracker` — plus a `ShutdownSignal` and a
`TaskTracker` for graceful teardown. The wiring lives in
`src/runtime.rs`:

```rust
// src/runtime.rs
pub struct ManagerGraph { /* all Arcs + channels_registry */ }
pub async fn build_managers(config: Arc<AuraConfig>, shutdown: ShutdownSignal, leak_detector: Arc<LeakDetector>) -> Result<ManagerGraph>;
pub struct RouterRunHandle { /* router, incoming_tx, incoming_rx, response_rx */ }
pub async fn wire_router(graph: &mut ManagerGraph) -> RouterRunHandle;
pub fn install_signal_handler(tracker: &mut TaskTracker, shutdown: ShutdownSignal);
pub async fn build_secret_vault(config: &AuraConfig) -> Result<Arc<SecretVault>>; // vault-only path, no manager graph
```

`gateway_cmd::start` is now the **only** caller of `build_managers` +
`wire_router` — the TUI talks to the gateway over loopback TCP and
holds no manager graph of its own. The gateway builds a
`GatewayServer` from `GatewayDeps`, binds the admin `TcpListener`
and a second loopback `TcpListener` (127.0.0.1:0) for the channel
WS, and drives them in parallel with the router via `tokio::select!`
on `shutdown.wait`. The channel listener publishes its chosen port
to `<workspace>/channel.port` (mode `0o600`) and the file is
unlinked on shutdown (plus a `Drop` guard covers panic exits).
`graph.channels_registry` starts empty at boot — every
registered channel (including the bundled TUI) arrives later as a
`/v1/channel-ws` client and registers itself from its route task.

`build_secret_vault` is a narrow helper that only opens the libsql
store far enough to construct a `SecretVault`. The TUI's remote boot
uses it to read the gateway token without touching the workspace
singleton lock; the vault-only `gateway token {show,rotate}`
subcommands use it for the same reason.

## Security and Observability

- **Secrets never logged unredacted.** Three layers: (a) `LeakDetector`
  rules registered at detector construction redact any log line that
  echoes the admin token or the per-start TUI token; (b) the admin
  auth middleware strips `?token=` from the URI before the
  `TraceLayer` span is emitted, and the channel middleware does the
  same to the channel-token query parameter; (c) the auth middlewares
  never echo header values.
- **Channel listener is loopback + same-UID.** The listener binds
  `127.0.0.1:0` and publishes the chosen port to
  `<workspace>/channel.port` at mode `0o600`, so a different local
  user can't even discover the port. The vault-token layer above is
  a *workspace binding*, not a defence against same-UID hostile
  processes — see the threat-model note under "Channel auth" for the
  boundary.
- **All gateway logs pass through `RedactingMakeWriter`** — the same
  writer wrapper the TUI uses (`src/logging.rs`). `AURA_LOG_FORMAT=json`
  is honoured. Request spans carry a `listener` field (`admin` /
  `channel`) so the origin of each request is obvious in logs.
- **Every mutation goes through a manager.** Route handlers call
  `SessionManager`, `JobManager`, `CronScheduler`, `MemoryManager` and
  friends directly; there are no side-channel writes that bypass
  Trace/Job observability.
- **Singleton lock applies only to the gateway.** `start` acquires
  the per-workspace lock on `<workspace>/aura.lock`. `aura tui` is a
  `/v1/channel-ws` client (see [`tui.md`](./tui.md)) and **does not**
  take the lock, so one long-lived `aura gateway` can serve many
  concurrent TUI sessions in the same workspace.

## Crate Layout

```
crates/gateway/
├── Cargo.toml               # features above, all deps via workspace = true
├── build.rs                 # emits $OUT_DIR/webui_assets.rs with include_bytes! + MIME per web/dist file
├── src/
│   ├── lib.rs               # re-exports GatewayServer, GatewayDeps, ChannelServer, …
│   ├── config.rs            # RuntimeGatewayConfig (admin bind + shutdown grace)
│   ├── server.rs            # GatewayDeps, AdminState, GatewayServer (admin TCP)
│   ├── channel_listener.rs  # ChannelServer (loopback TCP + channel.port discovery + accept loop)
│   ├── spawn.rs             # ChannelSpawner — spawn a subprocess client with a channel token
│   ├── channel/             # /v1/channel-ws WS server for sidecar plugins + the TUI
│   │   ├── mod.rs           #   module glue; re-exports route + state
│   │   ├── adapter.rs       #   Sidecar — per-connection Channel + outbound frame pump
│   │   ├── handshake.rs     #   validate_register (TUI vs. subprocess gating via AuthedClient)
│   │   ├── route.rs         #   ws_handler + inbound loop
│   │   └── state.rs         #   WsChannelState (registry, incoming_tx, tokens, sessions)
│   ├── auth/                # gateway auth surface (re-exports common types from mod.rs)
│   │   ├── mod.rs           #   re-exports AdminToken, AuthedClient, ChannelTokenTable, …
│   │   ├── admin.rs         #   AdminAuthState, AdminToken, require_admin_token
│   │   ├── channel.rs       #   ChannelAuthState, AuthedClient, require_channel_auth
│   │   └── token.rs         #   ChannelTokenTable + ClientIdentity + TokenHandle, vault-key + label consts
│   ├── error.rs             # GatewayError : IntoResponse
│   ├── test_support.rs      # cfg(test-support) build_test_deps + TestGateway
│   ├── api/
│   │   ├── mod.rs           # health::routes()
│   │   ├── dto.rs           # mirror DTOs + From<Domain> conversions — the only utoipa user
│   │   ├── openapi.rs       # GET /v1/openapi.json handler
│   │   ├── health.rs        # /healthz + /readyz (shared between listeners)
│   │   ├── webui.rs         # admin-fallback handler; include!s $OUT_DIR/webui_assets.rs
│   │   └── admin/           # mod.rs: v1_router_and_spec() → (Router, OpenApi), mounted on admin TCP
│   │       └── {status,config,jobs,cron,memory,traces,skills,tools,channels,llm}.rs
│   └── installer/
│       ├── mod.rs           # ServiceInstaller trait, InstallContext, ServiceStatus,
│       │                    # for_current_platform, resolve_exec_start
│       ├── systemd.rs       # cfg(all(target_os = "linux",  feature = "linux"))
│       └── launchd.rs       # cfg(all(target_os = "macos",  feature = "macos"))
└── tests/
    ├── auth.rs                  # admin bearer auth
    ├── admin_has_no_channels.rs # /v1/channel-ws is 404 on the admin listener
    ├── openapi_spec_sync.rs     # asserts router spec == docs/openapi.json (UPDATE_OPENAPI=1 to rewrite)
    └── channel_ws.rs            # full loopback-TCP WS round-trip via tokio-tungstenite
```

The frontend sources live outside the crate at `web/` (npm workspace,
not a Cargo member) and produce `web/dist/` which `build.rs` consumes.

The channel-listener wire constants (`CHANNEL_TOKEN_HEADER`, the
reserved `TUI_CLIENT_LABEL`, the vault key `TUI_TOKEN_VAULT_KEY`) and
the `ChannelTokenTable` / `ClientIdentity` / `TokenHandle` types live
inline in `crates/gateway/src/auth/token.rs` and are re-exported from
both `crate::auth` and the crate root, so internal call sites can
write `use crate::auth::ChannelTokenTable` and the bin's `gateway_cmd`
/ `tui_cmd` boot paths reach them as `aura_gateway::ChannelTokenTable`.
There is no compiled-in PSK any more — every credential is generated
at runtime and stored in the per-workspace vault.

## Collaboration

- **agent** — provides `SessionManager`, `JobManager`, `CronScheduler`,
  `MemoryManager`, `SecurityGateway`, `service::{ShutdownSignal,
  TaskTracker}` used by the server's graceful shutdown path.
- **channels** — the `Channel` handle, `IncomingMessage`,
  `OutgoingMessage`, `NoticeLevel`, `ChannelRegistry`, and the
  `wire::{Frame, Message}` MessagePack types the WS route speaks.
  Every accepted `/v1/channel-ws` connection registers its
  `Arc<Channel>` on the shared registry and unregisters on disconnect.
- **security** — `SecretVault` for admin-token + TUI-token persistence;
  `LeakDetector` for log redaction; `SecurityGateway` for the outgoing
  reveal path. The admin token and the per-start TUI token are
  registered as `LeakDetector::Replace` rules at detector-construction
  time.
- **config** — `AuraConfig::gateway` drives admin bind address, channel
  socket path, CORS origins, and shutdown grace.
- **storage** — `Store::open` for vault bootstrap in `gateway_cmd`; the
  `TraceStore` trait behind `/v1/traces/:session_id`.
- **tui** — the TUI is a `/v1/channel-ws` client like any other
  sidecar. It reads the per-start token from `gateway.tui_token` in
  the secret vault, presents it via the shared
  `x-aura-channel-token` header, registers as `channel_type = "tui"`,
  and client-generates session UUIDs. Admin endpoints are not
  reached from the TUI.
- **cli** — `Commands::Gateway { cmd: GatewayCmd }` is defined in
  `crates/cli/src/cli.rs`; the dispatcher explicitly returns
  `UnknownCommand` because `src/main.rs` intercepts the variant before
  it reaches dispatch.
- **bootstrap** — `src/main.rs` intercepts `Commands::Gateway` and calls
  `gateway_cmd::run`; the long-running `start` path spins up both
  listeners against the same manager graph.

[`aura_channels::Channel`]: ./channels.md
