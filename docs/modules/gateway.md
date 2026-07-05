# gateway - HTTP Server and Service Installer

## Overview

`baybo-gateway` is Baybo's headless backend. It runs **two listeners** side
by side against the same manager graph:

1. **Admin listener** — TCP, bearer-token authenticated. Surfaces the
   operator controls (config, jobs, cron, traces, skills, tools,
   channels-list, llm, status) that mirror the CLI command families, plus
   the `/v1/chat/*` web-chat family that backs the embedded React
   dashboard. The admin listener also **co-hosts** the admin/device-
   authed `/v1/channel-ws` + `/v1/blobs` subrouter (see
   `build_admin_router` in `src/server.rs`) so a browser chat tab loaded
   from the admin origin can open its WebSocket without discovering the
   loopback port. The co-hosted subrouter accepts the same admin bearer as
   the rest of the dashboard surface, or an approved device bearer when
   paired with `x-baybo-device-id`. The admin/channel split is about
   *which credential* gates a route, not a hard "no session data here"
   boundary.
2. **Channel listener** — loopback TCP (`127.0.0.1:<ephemeral>`),
   authenticated by a vault-issued channel token (TUI, subprocess
   sidecar, embedded tool sidecar). The chosen port is
   published to `<workspace>/state/channel.port`
   (mode `0o600`) so the TUI and spawned sidecars discover it without
   a config roundtrip. The listener hosts `GET /v1/channel-ws` (upgrades
   authed requests to a WebSocket) plus the blob endpoints (`POST
   /v1/blobs`, `GET /v1/blobs/{blob_id}`) via `channel::routes()` merging
   `blobs::routes()`. The built-in TUI and every out-of-process sidecar
   plugin speak this one protocol. `127.0.0.1` binding is hardcoded —
   config cannot loosen it to `0.0.0.0`.

The per-type [`baybo_channels::Channel`] is installed at gateway **boot**
from `ChannelsConfig` (`src/channel/boot.rs::install_channels`), not per
connection. Each accepted WS upgrade builds a `src/channel/adapter.rs::
Sidecar` that resolves the existing channel from the [`ChannelRegistry`]
and calls `Channel::attach(Connection)` (a lazy-install fallback only
covers out-of-tree sidecar types declared via `baybo.json` or test
fixtures that skipped boot). The `Sidecar` owns an outbound frame mpsc
and spawns two tasks: a translator that converts
[`SessionEvent`](baybo_channels::SessionEvent) → [`Frame`](baybo_channels::wire::Frame),
and a pump that drains the receiver onto the WS sink. Everything fans in
to the same mpsc, so the pump is the single serialisation point onto the
wire.

The gateway is driven by the `baybo gateway …` command tree. `start` runs
both listeners in the foreground; `install` / `enable` / `disable` /
`uninstall` / `status` manage the platform service unit; `token
{show|rotate}` manages the admin bearer token. The binary entrypoint
intercepts `Commands::Gateway` in `crates/baybo/src/main.rs` before the CLI dispatcher
and routes it to `crates/baybo/src/gateway_cmd.rs` — same pattern as `Commands::Tui`.

Configuration lives in `baybo_config::GatewayConfig` (a top-level
section on `BayboConfig`). The gateway owns its own bind address /
port; the `http` channel itself has no operator-facing knobs and is
unconditionally installed at boot.

## Design Decisions

### Two listeners, one router graph

Splitting the gateway separates credentials by blast radius: the admin
bearer gates the operator/dashboard surface, and a sidecar channel
plugin running as a child of the gateway holds only a channel token,
not the admin bearer, so it has no admin surface to hit even if
compromised. The split is **not** a hard "no session data on admin"
boundary — the admin listener serves the `/v1/chat/*` web-chat family
(create/list/get/hide session, transcript history) and co-hosts the
admin-bearer-authed `/v1/channel-ws` + `/v1/blobs` subrouter so the
browser dashboard can chat over the public admin bind without a second
credential.
Both listeners share the same manager graph (`SessionManager`,
`JobLifecycle`, …). The channel listener hosts `/v1/channel-ws` and
`/v1/blobs`; the router sees a single `IncomingMessage` stream
regardless of whether a frame came from the TUI, a sidecar, or a web
chat tab. The per-type [`baybo_channels::Channel`] each connection
attaches to is installed at boot and dispatched by `ChannelType` — no
parallel agent pipeline.

### Admin token, stored in `SecretVault`

On `baybo gateway enable`, `AdminToken::mint_if_absent` either reads
the current token or generates a fresh 32-byte random value (hex-
encoded) and writes it under the vault key `gateway.admin_token` — the
same AES-256-GCM store the rest of Baybo uses. `gateway.admin_token` is
the only key the code reads or writes for the admin bearer. `token show`
reads,
`token rotate` overwrites. `uninstall` removes the service unit but
leaves the token in the vault; `token rotate` is the explicit way to
invalidate a leaked token. Because the vault requires the master
encryption key, the token's confidentiality rides on the same root
secret as every other credential in the project.

### Channel auth — vault-issued tokens (TUI, subprocess, tool)

The channel listener enforces a single header on every request:
`x-baybo-channel-token: <hex>`, looked up against the in-memory
`ChannelTokenTable`. Each entry carries a `ClientIdentity { pid, label,
bound_channel_type }`; the auth middleware maps the entry's `label` to
one of three `auth::channel::AuthedClient` variants by reserved label /
prefix:

- **`Tui`** — label equals `baybo_gateway::TUI_CLIENT_LABEL` ("tui").
- **`Tool`** — label starts with `TOOL_CLIENT_LABEL_PREFIX` ("tool/",
  e.g. `tool/browser`). The embedded tool sidecars (the browser MCP
  server today). Session-scoped like the TUI, so it **bypasses the
  per-channel pairing gate on `/v1/blobs`**, and it is **rejected from
  the channel-WS handshake** — tool sidecars don't register channels.
- **`Subprocess`** — any other label (e.g. `sidecar-telegram`). Carries
  `pid`, `label`, and the bound `channel_type`.

The flavours of token that end up in the table:

- **TUI token.** Generated on every `baybo gateway start`, written to
  the secret vault under the key
  `baybo_gateway::TUI_TOKEN_VAULT_KEY` ("gateway.tui_token"), and
  registered with the gateway's own pid + the reserved TUI label. The
  gateway holds the returned `TokenHandle` for the entire lifetime of
  `start`, so the in-memory entry is revoked the moment shutdown
  drops it. The vault row stays around between starts (the next start
  overwrites it), but a TUI presenting a vault value from a previous
  generation is rejected because the in-memory table has the new
  value.
- **Subprocess capability tokens.** Minted inside
  `ChannelSpawner::spawn` before `Command::spawn`, handed to the
  child via the `BAYBO_CHANNEL_TOKEN` env var, and revoked when the
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
  `0o600` and `<workspace>/state/channel.port` is also `0o600`, so another
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

Token comparison differs by listener:

- **Admin bearer** is compared in constant time:
  `auth::admin::require_admin_token` runs the presented token through
  `baybo_gateway::constant_time_eq` against the expected value
  (`auth/admin.rs`).
- **Channel token** is *not* a constant-time compare, by design.
  `ChannelTokenTable::lookup` is a plain `DashMap::get(token)`
  (`auth/token.rs`) — the token is the map key, so matching it goes
  through the hash-map probe rather than a fixed-time byte comparison.
  This is an accepted trade-off, not a gap: the token is 256 random bits
  (hex-encoded) delivered over a loopback-only, same-UID surface, so a
  hash-probe timing side-channel on a high-entropy key is not a realistic
  recovery vector. Timing-safe equality is reserved for the admin bearer.

`baybo_gateway::constant_time_eq` is wired on the admin bearer path only,
deliberately per the above. `/healthz` and `/readyz` skip auth on both
listeners.

### Web/device chat auth — bearer plus identity claim

Browser chat tabs use the same admin bearer as the rest of the React
dashboard. REST calls carry `Authorization: Bearer <admin_token>`.
Browser `WebSocket` cannot set request headers, so the web client opens
`/v1/channel-ws?token=<admin_token>` on the admin listener; admin auth
validates the bearer, strips the query token before tracing, and marks
the request as `AuthedClient::Web`. The WebSocket Register handshake
then constrains that identity to the reserved `http` channel.

Direct device clients add `x-baybo-device-id: device-<hex(ed25519 pub)>` on every
`/v1/*` call. The header is only an identity claim: admin auth accepts it
only when the bearer is either the admin token or the matching approved
device `auth_token` in `DeviceStore`. Once accepted, the request is tagged
as `AuthedClient::Device { device_id }`, so chat REST is scoped to the
`device` channel, `/v1/channel-ws` must register as `device`, and `/v1/blobs`
stamps uploads with `device:<device_id>`.
Relay-mode device clients reach the same `v1_router_and_spec()` surface through
the Noise-authenticated API tunnel. After the IK device handshake, the
gateway injects `Authorization: Bearer <device auth_token>` plus
`x-baybo-device-id` before dispatching into the in-process router, so relay
and direct requests resolve identity through the same admin auth path.

The loopback channel listener remains channel-token-only for the TUI,
tool sidecars, and subprocess sidecars. The admin listener's co-hosted
channel subrouter no longer accepts web-specific channel tokens.

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
that converts `SessionEvent` → `Frame` and forwards it onto the same
mpsc. The channel already exists (installed at boot); `Sidecar::build`
resolves it from the registry and calls
`Channel::attach(Arc<Connection>)`, returning the `Sidecar` itself
(it does *not* hand an `Arc<Channel>` to a `register` call). The
approval gate lives on the `Channel`, not the connection — the boot-time
registry wiring installs a type-level gate into the shared
`ApprovalGateMap`. A single pump task drains the receiver and writes each
frame to the WS sink with
[`rmp_serde::to_vec_named`](baybo_channels::wire::encode) —
everything fans *in* to the mpsc, the pump is the only thing that
touches the socket. On disconnect `Sidecar::into_pump()` detaches the
connection from the channel and drops the last `frame_tx` clones, so the
pump exits cleanly without a separate stop signal.

### Tool approval over the sidecar WS

Each `Channel` is born at boot with its own `ChannelApprovalGate` +
`ApprovalQueue` (the `ApprovalSurface` on
`crates/channels/src/channel.rs`), built in `src/channel/boot.rs::
build_channel`. When a tool call hits the gate the entry is pushed on
the queue and the waker dispatches a `SessionEvent::ApprovalRequested`
through the channel's fan-out so every subscriber of the call's session
sees it. The client echoes a `ResolveApproval { call_id, decision }`
frame; the inbound loop calls `Sidecar::resolve_approval`, which
delegates to `Channel::resolve_approval` — that pops the matching queue
entry, reads the `session_id` off it, and broadcasts
`SessionEvent::ApprovalResolved` to every subscriber so all frontends
drop the prompt. The 5-minute timeout (`APPROVAL_TIMEOUT = 300s` in
`channel/boot.rs`) mirrors the TUI's original budget — long enough for a
human, short enough that forgotten prompts don't pin tool executors
forever.

Approvals resolve through the **channel-level** gate (`ApprovalGateMap`
keyed by `ChannelType`), with a second **session-level** tier
(`(ChannelType, SessionId)`) for session-scoped clients;
`ApprovalGateMap::get` tries session-level first, then type-level, then
falls back to the fail-closed `AutoDenyGate` (`crates/tools/src/
approval.rs`). Because the gate lives on the long-lived channel rather
than on a connection, a sidecar reconnecting does not lose pending
approvals — a resubscribe replays them (see the `Frame::Subscribe`
handler).

### Gateway owns its own DTOs — utoipa stays in the gateway

Route handlers call the manager `pub async fn` methods directly
(`SessionManager`, `JobLifecycle`, `CronScheduler`,
`TraceStore`, `SkillRegistry`, `ToolRegistry`) and serialise into DTOs
defined in `crates/gateway/src/api/dto.rs`. CLI handler output is not
reused — those are built around `CommandContext` + `OutputFormat` and
would force a cross-crate refactor of every `crates/cli/src/commands/`
module. The duplicated surface is small and the independence is worth
more than the line count saved.

`api/dto.rs` is also the **only** place in the workspace that depends
on `utoipa`. Domain crates (`baybo-model`, `baybo-job`, `baybo-cron`,
`baybo-tools`) stay HTTP-framework-agnostic — no `#[derive(ToSchema)]`,
no `#[schema(...)]` attributes leaking into them. The gateway defines
a mirror type for every domain type that appears on the wire and
provides `From<Domain>` conversions; handlers build DTOs at the seam
via `.map(DomainDto::from)`. Mirror types keep their bare name
(`Job`, `CronJob`, `ChannelType`, …) so the generated
OpenAPI schemas — and the TypeScript types downstream — are stable
across the refactor. Adding a field to a domain type now requires an
explicit edit in `dto.rs` to surface it on HTTP; that's a feature, not
a bug, because the drift test below will force the question.

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
baybo-gateway --test openapi_spec_sync` to rewrite the snapshot. A
drifted spec fails CI, which guarantees the frontend can regenerate
types without hitting a stale file.

The frontend consumes the spec through two tools:

The `/v1/logs` surface has two endpoints:

- `GET /v1/logs` — paged snapshot from `LogBuffer::query` (filter by
  `level`, `q`, `since`, `until`, `limit`, `offset`; `total` is
  independent of pagination).
- `GET /v1/logs/stream` — SSE stream subscribed to the same buffer's
  `broadcast::Sender`. The stream first emits an SSE comment (`: ready`)
  so the browser's `EventSource` fires `open` even on an idle system.
  Each captured record that matches the query params then ships as an
  `event: log` frame with `LogEntry` JSON as `data`. Back-pressure is
  bounded: a client that falls behind receives an `event: lagged` with
  the drop count, then resumes. Two terminal events also exist: an
  `event: error` (sent if a record fails to encode, then the stream
  ends) and an `event: end` (sent when the buffer's sender is dropped).
  The admin auth middleware accepts `?token=…` as a fallback because
  `EventSource` can't set headers; the webui's Live toggle uses it for
  real-time tail without polling.

- `openapi-typescript` (`npm run gen:api`, also wired into `npm run
  build`) reads `docs/openapi.json` and writes
  `app/web/src/api/schema.d.ts` — a pure `.d.ts` with `paths` and
  `components` maps. No runtime code is emitted.
- `openapi-fetch` consumes that schema type at runtime. The thin
  wrapper in `app/web/src/api/client.ts` (`createAdminClient({ baseUrl,
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
`pnpm --filter baybo-web build`; otherwise it reuses the existing
`app/web/dist/`. It then zstd-compresses each emitted asset and writes
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
`app/web/src/api/client.ts`, which wraps `openapi-fetch` with a
`Client<paths>` derived from the checked-in OpenAPI document. See the
"OpenAPI spec generation and the TypeScript client" design note above
for the regeneration flow.

Release flow:

```bash
pnpm install
pnpm --filter baybo-web build
cargo build --release -p baybo-gateway
```

If the tracked web inputs changed, `build.rs` attempts
`pnpm --filter baybo-web build` automatically during `cargo build`. If
that build fails or the frontend toolchain isn't available,
`build.rs` falls back to the existing `app/web/dist/`; if no dist exists at
all, it writes a one-line placeholder `index.html` so backend-only
development still compiles.

**Dev flow (HMR)**: rebuilding the gateway on every frontend tweak is
slow, so for UI iteration run the two sides separately:

```bash
# terminal 1 — debug gateway, admin listener on 127.0.0.1:8888
cargo run -- gateway start

# terminal 2 — Vite dev server with HMR on http://localhost:5173
cd web && npm run dev
```

`app/web/vite.config.ts` proxies `/v1`, `/healthz`, and `/readyz` to
`127.0.0.1:8888`, so the browser only talks to the Vite origin. This
avoids the CORS path entirely — `AdminAuthProvider` keeps `baseUrl =
window.location.origin` (the Vite origin), and bearer-token auth
still works end-to-end because the proxy forwards the `Authorization`
header. If you need to point the web bundle at a gateway on a
different origin (e.g. running `npm run dev` against a remote gateway),
add that origin to `gateway.cors_allowed_origins` in `baybo.json` and
use the LoginScreen's **Advanced → Gateway base URL** field to
override the stored `baseUrl`.

### Platform installers behind Cargo features

```
default      = ["linux", "macos"]
linux        = []              # systemd installer — renders ~/.config/systemd/user/baybo-gateway.service
macos        = []              # launchd installer — renders ~/Library/LaunchAgents/com.baybo.gateway.plist
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
`--exec-start` flag → `which baybo` on `$PATH` → `std::env::current_exe()`.
Under `cfg(debug_assertions)` with no flag the resolver **refuses** with
a clear hint: `target/debug/baybo` disappears after `cargo clean`, and
an installed service pointing at it would break silently. This is a
real footgun from early versions and the refusal is intentional.

`BAYBO_CONFIG_PATH` is captured at install time and embedded as
`Environment=BAYBO_CONFIG_PATH=...` in the systemd unit (or as an entry
under `EnvironmentVariables` in the launchd plist). systemd does not
inherit the invoking shell's env, so the capture is load-bearing.

### Service unit hardening — no restart-loop footgun

Both unit files include a small restart delay (`RestartSec=2s` on
systemd, `ThrottleInterval=2` on launchd). The per-workspace singleton
lock (`crates/baybo/src/singleton.rs`) will reject a second `start` invocation that
fires before the previous process has unwound; without the delay a
restart loop can thrash the lock. The systemd unit also sets
`TimeoutStopSec=30s` to match `GatewayConfig::shutdown_grace_secs`.

## CLI Surface

All commands live under `baybo gateway` and are also usable over slash
mode once a channel exposes them (not wired yet; the `Gateway` arm
returns `UnknownCommand` from the normal dispatcher because
`crates/baybo/src/main.rs` intercepts it before dispatch).

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

The handler lives in `crates/baybo/src/gateway_cmd.rs` and uses the `boot::` helpers
to build a throwaway `SecretVault` when a subcommand needs it. `start`
acquires the per-workspace singleton, opens the vault to read the admin
token and mint a fresh TUI token, builds the manager graph via
`runtime::build_managers` + `wire_router`, binds the admin and channel
listeners, and drives them under a shared `ShutdownSignal`.

## HTTP API

Routes are split between the two listeners. `/healthz` and `/readyz`
live on both; every other authenticated route goes through the
listener-specific auth middleware.

**Admin listener (TCP, bearer token)** — `auth::admin::require_admin_token`:

```
GET    /healthz                         liveness (no auth)
GET    /readyz                          managers ready (no auth)
GET    /v1/status                       baybo status mirror
GET    /v1/config                       redacted config snapshot (read-only)
PUT    /v1/config                       { path, value } → 200 { path, written_to, requires_restart }
DELETE /v1/config                       { path } → 200 { path, written_to, requires_restart }

POST   /v1/config/reload               re-read config; hot fields applied live → 200 ReloadOutcome
GET    /v1/jobs                         ?session=&status=&limit=&cursor=  (cursor pagination)
GET    /v1/jobs/:id
POST   /v1/jobs/:id/cancel

GET    /v1/cron
POST   /v1/cron                         { schedule, user_id, channel?, text, timezone, origin_session_id? }
GET    /v1/cron/:id
DELETE /v1/cron/:id

GET    /v1/traces                       ?status=&since=&until=&limit=&cursor=  filtered session-summary list
GET    /v1/traces/:session_id           session overview (message log + job summaries)
GET    /v1/traces/:session_id/jobs/:job_id   per-job step/span tree

GET    /v1/skills
GET    /v1/tools
GET    /v1/channels                     read-only registry list

GET    /v1/llm                          currently active provider/model
GET    /v1/llm/models                   configured LLM entries + effective settings
PUT    /v1/llm/models/:name             edit an entry (hot-reloaded in-process)
POST   /v1/llm/models/:name/test        probe the entry's provider
PUT    /v1/llm/default                  set default-llm (hot-reloaded)
GET    /v1/llm/usage                    ?since=&until=  per-entry usage aggregates

POST   /v1/chat/sessions                create http session
GET    /v1/chat/sessions                ?include_hidden=&include_cron=  newest-first list
GET    /v1/chat/sessions/:id            ?before_ordinal=&limit=  detail + transcript slice (+ last_llm pin)
PUT    /v1/chat/sessions/:id/model      pin this session's LLM (re-pins live actor; null ⇒ default-llm)
PUT    /v1/chat/sessions/:id/pin        pin/unpin (lifts to the sidebar's Pinned block)
PUT    /v1/chat/sessions/:id/folder     file into a folder (null ⇒ Uncategorized)
DELETE /v1/chat/sessions/:id            hide (row preserved); 204
POST   /v1/chat/sessions/:id/unhide     restore a hidden session
GET    /v1/chat/cron-messages           cron-fire sessions with prompt/response previews
GET    /v1/chat/slash-manifest          slash commands for the composer's /-autocomplete
GET    /v1/chat/folders                 the conversation-folder tree
POST   /v1/chat/folders                 create a folder
PATCH  /v1/chat/folders/:id             rename / reparent a folder
POST   /v1/chat/folders/:id/move        move a folder under a new parent
POST   /v1/chat/folders/reorder         reorder folders among siblings
DELETE /v1/chat/folders/:id             delete (dissolves; member sessions ⇒ Uncategorized); 204

GET    /v1/analytics                    aggregated tokens / cost / sessions over a time range
GET    /v1/logs                         paged snapshot from LogBuffer
GET    /v1/logs/stream                  SSE tail of the same buffer

GET    /v1/openapi.json                 live OpenAPI 3.1 document for the admin surface
```

`/v1/chat/*` is the web-chat family (`api/admin/chat.rs`, OpenAPI tag
`chat`). Despite the admin/device bearer in front of these routes, they
read and write session rows and transcripts — the admin/channel split is
by credential, not by "session data never touches admin". The web client
uses the admin bearer for REST, blob fetch/upload, and the co-hosted
`/v1/channel-ws` route on this same admin listener; direct device clients add the
device header and may use either the admin token or its approved device
token.

`GET /v1/chat/sessions/:id` returns a typed transcript: each
`ChatTranscriptItem` is either a `message` (user / final-assistant bubble)
or a `work` item — a reconstructed collapsed work block for a tool-using
turn. `reconstruct_transcript` folds the turn's persisted intermediate rows
(`Thinking` → reasoning, `ToolUse` + paired `ToolResult` → a tool step with
a re-derived summary and `ok`/`error`/`denied` status, mid-turn `Text` →
prose) into one item before the final answer, so a reload shows the same
`Worked Xs ›` block the live view did even though turn-progress events are
never persisted. The sidebar preview (`GET /v1/chat/sessions`) uses a single
indexed `load_last_user_message` lookup, so a prompt buried under a long
tool loop is still found. See `docs/turn-progress-events.md` for the
operator-only raw-tool-output disclosure and the message-only WS catch-up
caveat.

`GET /v1/chat/sessions/:id/catch-up?since_ordinal=N` is the forward reconnect
companion for device clients. It scans active rows above the cursor with the
same cap as WS replay, returns no partial slice when truncated, and interleaves
closed `work` items immediately before the matching final-assistant `message`.
The WS `Subscribe { since_ordinal }` path remains message-only; native merges
this API result into the same transcript stream after subscribe.

**Channel listener (loopback TCP, vault-issued tokens)** —
`auth::channel::require_channel_auth`:

```
GET    /healthz                         liveness (no auth)
GET    /readyz                          managers ready (no auth)

GET    /v1/channel-ws                   WebSocket upgrade (MessagePack frames)
POST   /v1/blobs                        upload non-text media → blob_id
GET    /v1/blobs/:blob_id               fetch media bytes by blob_id
```

The `/v1/channel-ws` + `/v1/blobs` handlers are mounted on **both**
listeners, but behind different auth middleware. The loopback channel
listener uses channel tokens for the TUI and subprocess sidecars. The
admin listener uses the admin/device bearer path: browser chat tabs with
only the admin token are marked `Web`, while direct device requests carrying
`x-baybo-device-id` are marked `Device` only if the bearer is the admin
token or that device's approved auth token.

The WS protocol is defined by [`baybo_channels::wire::Frame`]
(tagged on `kind`, MessagePack-named). The client opens with a
`Register { token, channel_type }` frame (no `protocol_version`; web
chat leaves the legacy `token` field empty because HTTP auth already
ran); the server validates it via `channel::handshake::validate_register` against
the [`auth::channel::AuthedClient`] the middleware already attached:
`Tui` → must claim `"tui"`; `Web` → must claim `"http"` (the only path
that may claim the reserved `http` type); `Device` → must claim `"device"`;
`Subprocess` → must match the minted identity's `(pid, label)`, respect
its `bound_channel_type`, and claim a non-reserved type; `Tool` →
rejected outright (tool sidecars don't register channels). The validator returns only the channel type —
per-session interest is negotiated **after** the handshake via
`Subscribe`, so `Register` carries no `session_id`.

The full frame set (see `crates/channels/src/wire.rs`):

- **Handshake / lifecycle:** `Register`, `RegisterAck { ok, reason? }`,
  `Reset { reason }`, `Ping`, `Pong`.
- **Subscription (Subscribed-kind only):** `Subscribe { session_id,
  since_ordinal? }`, `Unsubscribe { session_id }`. `Multiplexed`
  channels (telegram/weixin/discord) auto-wildcard and ignore these.
- **Messages:** `Message` (user input in; agent's final response or an
  echo of inbound to other subscribers out), `Messages { messages }` (a
  client → server **atomic batch** — the web "send all queued at once"
  path, coalesced into one turn), `AnswerDelta` (incremental answer text,
  server → client), `Notice` (out-of-band warn/error), `Attachment
  { session_id, user_id, attachments }` (mid-turn tool-emitted media).
- **Turn progress (server → client):** `Reasoning` (incremental thinking),
  `ToolStarted` / `ToolCompleted` (tool-call lifecycle), `TaskList { items }`
  (the agent's live task-checklist), `TurnState
  { active, started_at? }` (is a turn in flight). Both edges are projected
  from the job store by `spawn_turn_state_projector` (subscribed to the job
  lifecycle bus, which carries the `start` edge and the terminal edges), and
  one snapshot is sent per `Subscribe` from the same `active_turn_started_at`
  read — so a late-joining tab learns about a turn whose progress frames it
  missed, and the actor never emits this frame. Streaming clients (TUI / web)
  render these live; clients without a partial surface drop them.
  See [`docs/turn-progress-events.md`](../turn-progress-events.md).
- **Approvals:** `ApprovalRequested` / `ApprovalResolved` (server →
  client), `ResolveApproval` (client → server), `PendingApprovalsSnapshot
  { session_id, call_ids }` (server → client, on Subscribe).
- **TUI history:** `HistorySnapshot { session_id, entries }`,
  `HistoryAppend { session_id, entry }`.
- **Bot multiplexing (Multiplexed channels):** `StartBot`, `StopBot`
  (server → client), `BotStatus` (client → server), `SlashManifest`
  (server → client).
- **Chat session signalling (http + device subscribed channels):** `SessionUpdated
  { session_id, patch }` (the `SessionPatch` carries Create/Hide/Unhide
  plus `pinned` and `folder_id` changes), `SessionActivity { session_id,
  source, at }`, `FoldersChanged { folders }` (a full folder-tree snapshot
  re-broadcast after any folder mutation).

A note on `AnswerDelta`: `agent_output_to_frame` maps
`AgentEvent::AnswerDelta` to `Frame::AnswerDelta` and the
`translator_loop` sends it live on the wire (no coalescing) — the TUI
and web chat stream the reply prose from it. Clients without a partial
surface (multiplexed sidecars) ignore it and render the final `Message`.
The same holds for the `Reasoning` / `ToolStarted` / `ToolCompleted`
turn-progress frames.

Session resolution is **not** "create on first message via
`SessionManager::get_or_create`". It depends on the channel kind:

- **Subscribed** channels (tui, http): the connection must first
  `Subscribe` to a client-named `session_id`; an inbound `Message` for a
  session the connection isn't subscribed to is dropped
  (`resolve_inbound_session` in `channel/route.rs`). The TUI generates
  its own session UUID and subscribes to it.
- **Multiplexed** channels (telegram, …): the server derives the session
  from `(channel_type, user_id)` via
  `ChannelSessionResolver::resolve_or_create`, but only after a pairing
  gate passes; sidecar-supplied `session_id`s are ignored.

For TUI clients only, the input-history ring rides two frames:
`HistorySnapshot { session_id, entries }` is sent **inside the
`Frame::Subscribe` handler**, gated on `channel_type == "tui"` (it must
be the next frame after the per-session subscribe, before any catch-up
replay), and `HistoryAppend { session_id, entry }` is sent by the
client after every accepted submission. The gateway owns the encrypted
ring via `channel::TuiHistoryStore` (vault key `baybo.tui.input_history`,
500-entry cap, consecutive-duplicate dedup); concurrent appends from
multiple TUIs on the same gateway serialise through a
`tokio::sync::Mutex` on the store. TUI clients never open the vault
themselves. See [`tui.md`](./tui.md) and [`security.md`](./security.md).

Config mutation endpoints (`PUT /v1/config`, `DELETE /v1/config`, `PUT
/v1/llm/...`) write through to the same on-disk `baybo.json` that `baybo
config set/unset` targets, then trigger an **in-process reload** via the
shared `ConfigReloader` (`crates/baybo/src/reload.rs`, re-exported from `lib.rs`).
Hot-updatable fields take effect live; only a non-hot field forces a
restart, surfaced as `requires_restart: true` (the reloader reports
`ReloadError::NotHotReloadable`, which the handler maps to "true", not an
error). `POST /v1/config/reload` re-reads the file on demand and returns
a `ReloadOutcome`. If the gateway was booted without a config path
(pure-default boot), the mutation endpoints return `400 Bad Request`.

## Runtime Assembly

`start` builds the full manager graph — `SessionManager`,
`JobLifecycle`, `CronScheduler`, `TraceStore`,
`SecurityGateway`, `SkillRegistry`, `ToolRegistry`, `ToolExecutor`,
`SkillAssessor`, the LLM stack (`llm_client: Arc<BillableLlm>` plus the
`llm_pool: LlmPoolHandle` it's the default client of), `WorkspaceManager`,
`LeakDetector`, `ChannelRegistry`, `CostManager` — plus a
`ShutdownSignal` and a `TaskTracker` for graceful teardown. The wiring
lives in `crates/baybo/src/runtime.rs` (the binary crate's `src/`, not under
`crates/gateway/`):

```rust
// crates/baybo/src/runtime.rs
pub struct ManagerGraph { /* all Arcs; llm_client + llm_pool; channels_registry; … */ }
pub async fn build_managers(
    config: Arc<BayboConfig>,
    config_path: Option<PathBuf>,
    shutdown: ShutdownSignal,
    leak_detector: Arc<LeakDetector>,
    embedded_mcp_servers: Vec<EmbeddedMcpServer>,
) -> anyhow::Result<ManagerGraph>;
pub struct RouterRunHandle { /* router, incoming_tx, incoming_rx, response_rx */ }
pub async fn wire_router(graph: &mut ManagerGraph) -> RouterRunHandle;
pub fn install_signal_handler(tracker: &mut TaskTracker, shutdown: ShutdownSignal);
pub async fn build_secret_vault(config: &BayboConfig) -> anyhow::Result<Arc<SecretVault>>; // vault-only path, no manager graph
```

`gateway_cmd::start` is the boot-path caller of `build_managers` +
`wire_router` — the TUI talks to the gateway over loopback TCP and
holds no manager graph of its own. The `embedded_mcp_servers` arg lets
the gateway pre-assemble in-process MCP servers (the browser tool
sidecar's blob-upload bridge, etc.); non-gateway callers pass an empty
`Vec`. `GatewayDeps`/`AdminState` carry `llm_pool`; the admin LLM
handlers read the live pool's `default_client()` so a hot-reload is
reflected. The gateway builds a `GatewayServer` from `GatewayDeps`, binds
the admin `TcpListener` and a second loopback `TcpListener` (127.0.0.1:0)
for the channel WS, and drives them in parallel with the router via
`tokio::select!` on `shutdown.wait`. The channel listener publishes its
chosen port to `<workspace>/state/channel.port` (mode `0o600`) and the
file is unlinked on shutdown (plus a `Drop` guard covers panic exits).
`graph.channels_registry` is **populated at boot** by
`channel::boot::install_channels` (one `Channel` per enabled channel
type, including the always-on `http` channel and the bundled TUI when
`cli.enabled`); incoming `/v1/channel-ws` connections then `attach` to
the channel for their type rather than installing it.

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
  `<workspace>/state/channel.port` at mode `0o600`, so a different local
  user can't even discover the port. The vault-token layer above is
  a *workspace binding*, not a defence against same-UID hostile
  processes — see the threat-model note under "Channel auth" for the
  boundary.
- **All gateway logs pass through `RedactingMakeWriter`** — the same
  writer wrapper the TUI uses (`src/logging.rs`). `BAYBO_LOG_FORMAT=json`
  is honoured. Request spans carry a `listener` field (`admin` /
  `channel`) so the origin of each request is obvious in logs.
- **Every mutation goes through a manager.** Route handlers call
  `SessionManager`, `JobLifecycle`, `CronScheduler` and
  friends directly; there are no side-channel writes that bypass
  Trace/Job observability.
- **Singleton lock applies only to the gateway.** `start` acquires
  the per-workspace lock on `<workspace>/baybo.lock`. `baybo tui` is a
  `/v1/channel-ws` client (see [`tui.md`](./tui.md)) and **does not**
  take the lock, so one long-lived `baybo gateway` can serve many
  concurrent TUI sessions in the same workspace.

## Crate Layout

```
crates/gateway/
├── Cargo.toml               # features above, all deps via workspace = true
├── build.rs                 # three pipelines: (1) pnpm install; (2) WebUI → $OUT_DIR/webui_assets.rs;
│                            #   (3) sidecars → $OUT_DIR/sidecar_assets.rs (bun build for sidecars/channel/*,
│                            #   esbuild+node for sidecars/tool/*). All assets zstd-compressed + include_bytes!.
├── src/
│   ├── lib.rs               # re-exports GatewayServer, GatewayDeps, ChannelServer, ConfigReloader, Sidecar*, …
│   ├── config.rs            # RuntimeGatewayConfig (admin bind + shutdown grace + CORS)
│   ├── server.rs            # GatewayDeps, AdminState, ChannelState, GatewayServer; build_admin_router
│   │                        #   (admin TCP + co-hosted /v1/channel-ws + /v1/blobs subrouter)
│   ├── channel_listener.rs  # ChannelServer (loopback TCP + channel.port discovery + accept loop)
│   ├── spawn.rs             # ChannelSpawner — spawn a subprocess client with a channel token
│   ├── reload.rs            # ConfigReloader trait + ReloadOutcome/ReloadError (in-process config hot-reload)
│   ├── log_buffer.rs        # LogBuffer ring + tracing Layer behind /v1/logs[/stream]
│   ├── channel/             # /v1/channel-ws WS server + /v1/blobs for sidecars, TUI, and web chat
│   │   ├── mod.rs           #   module glue + re-exports; notes every SessionEvent is sent live 1:1 (no coalescing)
│   │   ├── adapter.rs       #   Sidecar — SessionEvent→Frame translator + outbound pump; attaches to Channel
│   │   ├── blobs.rs         #   POST /v1/blobs + GET /v1/blobs/{id}
│   │   ├── boot.rs          #   install_channels / build_channel; APPROVAL_TIMEOUT=300s; per-channel gate
│   │   ├── bot_reconciler.rs#   reconciles StartBot/StopBot rosters to Multiplexed sidecars
│   │   ├── control.rs       #   ChannelControlRegistry (push control frames from outside the route task)
│   │   ├── dedup.rs         #   InboundDedup (recent-window (channel,bot,platform_msg_id) dedup)
│   │   ├── handshake.rs     #   validate_register (Tui/Tool/Subprocess/Web gating via AuthedClient)
│   │   ├── history.rs       #   TuiHistoryStore (vault-backed TUI input-history ring)
│   │   ├── route.rs         #   ws_handler + inbound loop (Subscribe/Message/ResolveApproval/…)
│   │   ├── session_pulse.rs #   http-channel dispatch observer → throttled Frame::SessionActivity
│   │   ├── session_resolver.rs # ChannelSessionResolver (Multiplexed (channel,user)→session)
│   │   ├── slash.rs         #   slash-command manifest + sidecar slash handling
│   │   └── state.rs         #   WsChannelState (registry, tokens, stores, pairing, …)
│   ├── sidecar/             # embedded JS sidecar packaging + supervision
│   │   ├── mod.rs           #   SidecarRuntime / SidecarSupervisor, BUN/NODE binary env, domains
│   │   ├── assets.rs        #   include!s $OUT_DIR/sidecar_assets.rs; materialises bundles to disk
│   │   ├── embedded_mcp.rs  #   EmbeddedMcpServer entries (browser tool blob-upload bridge, …)
│   │   ├── pipe_pump.rs     #   drains sidecar stdout/stderr NDJSON into the LogBuffer
│   │   └── supervisor.rs    #   spawn/restart lifecycle for in-tree sidecars
│   ├── auth/                # gateway auth surface (re-exports common types from mod.rs)
│   │   ├── mod.rs           #   re-exports AdminToken, AuthedClient, ChannelTokenTable, …
│   │   ├── admin.rs         #   AdminAuthState, AdminToken, require_admin_token (constant_time_eq compare)
│   │   ├── channel.rs       #   ChannelAuthState, AuthedClient {Tui,Tool,Subprocess,Web}, require_channel_auth
│   │   └── token.rs         #   ChannelTokenTable (DashMap lookup) + ClientIdentity + TokenHandle, consts
│   ├── error.rs             # GatewayError : IntoResponse
│   ├── test_support.rs      # cfg(test-support) build_test_deps + TestGateway
│   ├── api/
│   │   ├── mod.rs           # health::routes()
│   │   ├── dto.rs           # mirror DTOs + From<Domain> conversions — the only utoipa user
│   │   ├── openapi.rs       # GET /v1/openapi.json handler
│   │   ├── health.rs        # /healthz + /readyz (shared between listeners)
│   │   ├── webui.rs         # admin-fallback handler; include!s $OUT_DIR/webui_assets.rs
│   │   └── admin/           # mod.rs: v1_router_and_spec() → (Router, OpenApi), mounted on admin TCP
│   │       └── {status,config,jobs,cron,traces,analytics,skills,tools,channels,chat,llm,logs}.rs
│   └── installer/
│       ├── mod.rs           # ServiceInstaller trait, InstallContext, ServiceStatus,
│       │                    # for_current_platform, resolve_exec_start
│       ├── systemd.rs       # cfg(all(target_os = "linux",  feature = "linux"))
│       └── launchd.rs       # cfg(all(target_os = "macos",  feature = "macos"))
└── tests/                   # all.rs aggregates the rest into one binary
    ├── auth.rs                  # admin bearer auth
    ├── admin_has_no_channels.rs # asserts 404 on /v1/sessions*/* approvals* (routes that never existed)
    ├── channel_ws.rs            # full loopback-TCP WS round-trip via tokio-tungstenite
    ├── chat_api.rs              # /v1/chat/* web-chat session + token-mint surface
    ├── jobs_pagination.rs       # /v1/jobs cursor pagination + filters
    ├── llm_endpoint.rs          # /v1/llm* models/default/usage surface
    ├── logs_endpoint.rs         # /v1/logs + /v1/logs/stream SSE
    └── openapi_spec_sync.rs     # asserts router spec == docs/openapi.json (UPDATE_OPENAPI=1 to rewrite)
```

The frontend sources live outside the crate at `web/` (pnpm workspace,
not a Cargo member) and produce `app/web/dist/` which `build.rs` consumes;
the in-tree JS sidecars live at `sidecars/channel/*` and `sidecars/tool/*` and are
bundled into `$OUT_DIR/sidecar_assets.rs`. Note `crates/baybo/src/runtime.rs` and
`crates/baybo/src/gateway_cmd.rs` (the boot path) live in the **binary** crate's
`src/`, not in `crates/gateway/`.

The channel-listener wire constants (`CHANNEL_TOKEN_HEADER`, the
reserved `TUI_CLIENT_LABEL`, the vault key `TUI_TOKEN_VAULT_KEY`) and
the `ChannelTokenTable` / `ClientIdentity` / `TokenHandle` types live
inline in `crates/gateway/src/auth/token.rs` and are re-exported from
both `crate::auth` and the crate root, so internal call sites can
write `use crate::auth::ChannelTokenTable` and the bin's `gateway_cmd`
/ `tui_cmd` boot paths reach them as `baybo_gateway::ChannelTokenTable`.
There is no compiled-in PSK any more — every credential is generated
at runtime and stored in the per-workspace vault.

## Collaboration

- **agent** — provides `SessionManager`, `JobLifecycle`, `CronScheduler`,
  `SecurityGateway`, `service::{ShutdownSignal,
  TaskTracker}` used by the server's graceful shutdown path.
- **channels** — the `Channel` / `Connection` / `SessionEvent` handles,
  `IncomingMessage`, `NoticeLevel`, `ChannelRegistry`, and the
  `wire::{Frame, Message}` MessagePack types the WS route speaks. The
  per-type `Channel` is installed once at boot; every accepted
  `/v1/channel-ws` connection `attach`es a `Connection` to its channel
  and `detach`es on disconnect (the `Channel` itself stays in the
  registry for the gateway's lifetime).
- **security** — `SecretVault` for admin-token + TUI-token persistence;
  `LeakDetector` for log redaction; `SecurityGateway` for the outgoing
  reveal path. The admin token and the per-start TUI token are
  registered as `LeakDetector::Replace` rules at detector-construction
  time.
- **config** — `BayboConfig::gateway` drives admin bind address, channel
  socket path, CORS origins, and shutdown grace.
- **storage** — `Store::open` for vault bootstrap in `gateway_cmd`; the
  `TraceStore` trait behind `/v1/traces/:session_id`.
- **tui** — the TUI is a `/v1/channel-ws` client like any other
  sidecar. It reads the per-start token from `gateway.tui_token` in
  the secret vault, presents it via the shared
  `x-baybo-channel-token` header, registers as `channel_type = "tui"`,
  and client-generates session UUIDs. Admin endpoints are not
  reached from the TUI.
- **cli** — `Commands::Gateway { cmd: GatewayCmd }` is defined in
  `crates/cli/src/cli.rs`; the dispatcher explicitly returns
  `UnknownCommand` because `crates/baybo/src/main.rs` intercepts the variant before
  it reaches dispatch.
- **bootstrap** — `crates/baybo/src/main.rs` intercepts `Commands::Gateway` and calls
  `gateway_cmd::run`; the long-running `start` path spins up both
  listeners against the same manager graph.

[`baybo_channels::Channel`]: ./channels.md
