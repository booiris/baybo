# gateway - HTTP Server and Service Installer

## Overview

`aura-gateway` is Aura's headless backend. It runs **two listeners** side
by side against the same manager graph:

1. **Admin listener** — TCP, bearer-token authenticated. Surfaces the
   operator controls (config, jobs, cron, memory, traces, skills, tools,
   channels-list, llm, status) that mirror the CLI command families. No
   chat content or session data flows here.
2. **Channel listener** — Unix domain socket, authenticated by either the
   TUI pre-shared key or a per-subprocess token (subprocess pid pinned).
   Serves session CRUD, message submit, per-session SSE, and tool
   approvals. This is the listener the TUI and future sidecar channel
   plugins talk to.

The gateway also implements [`aura_channels::ChannelAdapter`] for
`ChannelType::Http` so messages submitted over the channel listener flow
through the same router path as TUI/telegram/discord — no parallel agent
pipeline.

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
graph (`SessionManager`, `JobManager`, …) and the same
`HttpAdapter` — routing and observability stay uniform. The adapter
contract (`send(AgentOutput)`, `approval_gate`) goes through
`ChannelRegistry` dispatch exactly as TUI/telegram/discord do;
`POST /v1/sessions/:id/messages` rebuilds the
`User` with `ChannelType::Http` before submission so outgoing events
route back through the same channel.

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

### Channel auth — peer-cred + PSK / subprocess token

The channel listener enforces two stacked checks on every request:

1. **SO_PEERCRED** (Linux) / `getpeereid` (macOS) extracts the peer
   `(uid, pid)` on accept. Requests from a uid other than
   `geteuid()` are rejected before middleware runs.
2. **Header auth** — exactly one of:
   - `x-aura-tui-secret: <hex>` matching the effective TUI PSK (see
     below). Used by `aura tui`.
   - `x-aura-channel-token: <hex>` matching an entry in the in-memory
     `ChannelTokenTable`, provided the connecting peer's pid matches
     the pid recorded when the token was minted. Used by subprocess
     channel plugins spawned via `ChannelSpawner`.

The TUI PSK lives in `aura-gateway-auth` as a 32-byte value generated
at build time (`build.rs` writes `$OUT_DIR/tui_psk.bin`) plus an
optional per-install salt: on first start the gateway writes 32 random
bytes to `{workspace_identity_dir}/tui_psk.salt` (mode 0600) and
derives the effective PSK via `HKDF-SHA256(EMBEDDED_PSK, salt)`. TUI
reads the same salt file. Two users running the same release binary
end up with different effective PSKs.

#### TUI PSK threat model — what it is and isn't

The effective TUI PSK is a **workspace-binding token**, *not* a defence
against a same-UID hostile process. Being explicit about the boundary:

*Designed to reject*
- connections from a different Unix user (already covered by UDS
  `0o600` perms + `SO_PEERCRED`; the PSK is the belt on top of those
  suspenders);
- cross-workspace mix-ups — a TUI built against workspace A cannot
  authenticate to a gateway in workspace B because the per-install
  salt diverges;
- stale reconnects from an older build / a different on-disk install
  (different embedded PSK bytes).

*Not designed to defend against*
- a malicious process running as the same UID as the gateway. Such a
  process can read the salt file and the installed binary, recompute
  the effective PSK, and present it. Treat "same-UID local adversary"
  as out of scope for this mechanism.

The practical tools for the same-UID threat are file permissions on
the binary + salt file, service-manager sandboxing, and OS-level
isolation (seccomp, sandbox-exec, etc.) — not an application-layer
PSK.

Per-subprocess tokens are minted by `ChannelSpawner::spawn` before
`Command::spawn`, inserted into `ChannelTokenTable` keyed by the hash
of the token plus the child pid, and removed in the `ChildHandle`
`Drop`. Pid reuse is bounded by the child's lifetime. The same
threat-model caveat applies: a same-UID process can read
`/proc/<pid>/environ` on Linux and lift the token, so subprocess
tokens are also workspace/lifetime binding, not hostile-process
resistance.

All header compares are constant-time via `subtle::ConstantTimeEq`.
`/healthz` and `/readyz` skip auth on both listeners.

### Admin auth — bearer token, URI sanitisation before tracing

`auth_admin::require_admin_token` accepts `Authorization: Bearer
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
The workable option is to **read the vault-stored admin token in
`runtime::build_managers` before the detector is built** and add it as a
rule on the fresh detector, then also register the effective TUI PSK and
any active channel tokens after `ChannelTokenTable` is created. If the
vault has not been seeded yet (first `start` before `enable`), the
runtime fails fast with a message telling the user to run
`aura gateway enable` first. This means `enable` needs the full storage
init path (open `Store`, open `SecretVault`), not just a one-shot file
write.

### Adapter owns SSE fan-out

`HttpAdapter` keeps a `DashMap<SessionId, broadcast::Sender<SseEvent>>`
plus a `OnceLock<mpsc::Sender<IncomingMessage>>` captured during
`ChannelAdapter::start`. `subscribe` returns a receiver (creating the
broadcast channel on first use); `submit` reads the `OnceLock` to push
an `IncomingMessage` into the router. The single outbound entrypoint
`ChannelAdapter::send(AgentOutput)` matches the variant (`Delta`,
`Message`, `Notice`) and fans out to the matching broadcast sender.
`broadcast::Sender::send` is lossy only when a subscriber is lagging
past the buffer — in that case `BroadcastStream` surfaces `Lagged` to
the SSE handler, which emits an `error` event and continues. `stop`
clears the map, which drops all senders and signals EOF on every live
stream; the `incoming_tx` slot stays set for the adapter's lifetime
(mpsc EOF propagates when the adapter itself is dropped).

### Approval over HTTP

`HttpAdapter::approval_gate()` returns a `ChannelApprovalGate` backed by
a gateway-scoped `ApprovalQueue` shared with the `/v1/approvals*` REST
handlers. When a tool call hits the gate, the entry is queued and a
`ApprovalEvent::Added` is broadcast on the gateway-wide approval
stream; clients read the pending list from `GET /v1/approvals` and
resolve individual entries via `POST /v1/approvals/:call_id { decision }`.
The waker closure captured at gate construction fires synchronously
after the push, so the SSE notification is enqueued before the gate's
wait-for-decision future starts blocking. The timeout mirrors the
TUI's 5 minutes — long enough for a human, short enough that forgotten
prompts don't pin tool executors forever.

Approvals key on `ChannelType::Http`, not on a specific chat session,
so they live on a standalone SSE endpoint (`/v1/approvals/stream`)
rather than the per-session stream. Any frontend can resolve any
entry: a resolution POSTed by one client is broadcast as
`ApprovalEvent::Resolved` so other connected clients drop the entry
from their UI.

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
`web/dist/` at compile time and emits `$OUT_DIR/webui_assets.rs` with
one `match` arm per asset — each arm pairs an `include_bytes!`
reference with a pre-computed MIME string from a small hand-rolled
extension table (`html`, `js`, `css`, `svg`, `png`, `ico`, fonts,
fallbacks). No `rust-embed`, no `mime_guess`; the handler reduces to a
two-entry lookup.

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
cd web
npm ci
npm run build
cargo build --release -p aura-gateway
```

If the frontend hasn't been built, `build.rs` drops a one-line
placeholder `index.html` into `web/dist/` so `cargo build` still
works for backend-only development. `cargo:rerun-if-changed=web/dist`
makes the macro re-fire on the next `npm run build`.

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

**Admin listener (TCP, bearer token)** — `auth_admin::require_admin_token`:

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

**Channel listener (UDS, peer-cred + PSK/token)** —
`auth_channel::require_channel_client`:

```
GET    /healthz                         liveness (no auth)
GET    /readyz                          managers ready (no auth)

GET    /v1/sessions
POST   /v1/sessions                     { user_id?, name? }
GET    /v1/sessions/:id
DELETE /v1/sessions/:id                 soft-delete
GET    /v1/sessions/:id/messages        history

POST   /v1/sessions/:id/messages        { text } → 202 { message_id }
GET    /v1/sessions/:id/stream          SSE: delta | response | notice | end

GET    /v1/approvals                    list pending approvals
POST   /v1/approvals/:call_id           { decision: "approve" | "deny" } → 200 { call_id, decision }
GET    /v1/approvals/stream             SSE: added | resolved | end
```

Hitting a channel route on the admin listener returns `404` — the route
is not mounted there at all, so a leaked admin token yields nothing. The
`admin_has_no_channels` integration test enforces this.

### SSE event schema

Per-session stream (`/v1/sessions/:id/stream`):

```
event: delta
data: { "kind": "delta",    "text": "partial assistant text" }

event: response
data: { "kind": "response", "text": "final assistant text for the turn" }

event: notice
data: { "kind": "notice",   "level": "warn" | "error", "text": "..." }

event: end
data:
```

Approval stream (`/v1/approvals/stream`):

```
event: added
data: { "kind": "added",    "call_id": "...", "session_id": "...",
        "tool": "...", "accesses": [...], "params_preview": "..." }

event: resolved
data: { "kind": "resolved", "call_id": "...",
        "decision": "approve" | "deny" }

event: end
data:
```

Mutation endpoints (`PUT /v1/config`, `DELETE /v1/config`) write
through to the same on-disk `aura.json` that `aura config set/unset`
targets. The in-memory `Arc<AuraConfig>` held by managers is **not**
swapped; `requires_restart: true` in the response signals that the
gateway must be restarted for the change to take effect. If the
gateway was booted without a config path (pure-default boot), both
endpoints return `400 Bad Request`.

`KeepAlive` sends `ping` comments every 15 s so browser/proxy buffers
don't reset the connection. A lagged consumer receives a single `error`
event and the stream continues.

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
`wire_router` — the TUI talks to the gateway over UDS and holds no
manager graph of its own. The gateway registers the `HttpAdapter` into
`graph.channels_registry`, builds a `GatewayServer` from
`GatewayDeps`, binds both the admin `TcpListener` and the channel
`UnixListener`, and drives them in parallel with the router via
`tokio::select!` on `shutdown.wait`. The channel UDS is `chmod 0o600`
after bind and unlinked on shutdown (plus a `Drop` guard covers panic
exits).

`build_secret_vault` is a narrow helper that only opens the libsql
store far enough to construct a `SecretVault`. The TUI's remote boot
uses it to read the gateway token without touching the workspace
singleton lock; the vault-only `gateway token {show,rotate}`
subcommands use it for the same reason.

## Security and Observability

- **Secrets never logged unredacted.** Three layers: (a) `LeakDetector`
  rules registered at detector construction redact any log line that
  echoes the admin token, the effective TUI PSK, or an active channel
  token; (b) the admin auth middleware strips `?token=` from the URI
  before the `TraceLayer` span is emitted; (c) the channel auth
  middleware never echoes header values.
- **UDS is same-UID only.** The socket is `srw-------` (0o600) and
  `SO_PEERCRED` rejects non-matching uids before any header is looked
  at. A process running as a different local user cannot connect even
  with a valid PSK. The PSK layer above is a *workspace binding*, not a
  defence against same-UID hostile processes — see the PSK threat
  model note under "Channel auth" for the boundary.
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
  UDS+SSE client (see [`tui.md`](./tui.md)) and **does not** take
  the lock, so one long-lived `aura gateway` can serve many
  concurrent TUI sessions in the same workspace.

## Crate Layout

```
crates/gateway/
├── Cargo.toml               # features above, all deps via workspace = true
├── build.rs                 # emits $OUT_DIR/webui_assets.rs with include_bytes! + MIME per web/dist file
├── src/
│   ├── lib.rs               # re-exports GatewayServer, GatewayDeps, ChannelServer, …
│   ├── config.rs            # RuntimeGatewayConfig (admin bind + channel socket path + shutdown grace)
│   ├── server.rs            # GatewayDeps, AdminState, GatewayServer (admin TCP)
│   ├── uds.rs               # cfg(unix) ChannelServer (channel UDS + accept loop)
│   ├── spawn.rs             # cfg(unix) ChannelSpawner — spawn a subprocess client with a channel token
│   ├── http_adapter.rs      # HttpAdapter : ChannelAdapter + SseEvent
│   ├── auth_admin.rs        # AdminAuthState, require_admin_token
│   ├── auth_channel.rs      # ChannelAuthState, require_channel_client (peer-cred + PSK/token)
│   ├── error.rs             # GatewayError : IntoResponse
│   ├── test_support.rs      # cfg(test-support) build_test_deps + TestGateway
│   ├── api/
│   │   ├── mod.rs           # health::routes()
│   │   ├── dto.rs           # mirror DTOs + From<Domain> conversions — the only utoipa user
│   │   ├── openapi.rs       # GET /v1/openapi.json handler
│   │   ├── health.rs        # /healthz + /readyz (shared between listeners)
│   │   ├── webui.rs         # admin-fallback handler; include!s $OUT_DIR/webui_assets.rs
│   │   ├── admin/           # mod.rs: v1_router_and_spec() → (Router, OpenApi), mounted on admin TCP
│   │   │   └── {status,config,jobs,cron,memory,traces,skills,tools,channels,llm}.rs
│   │   └── channel/         # mod.rs exposes v1_router() mounted on the channel UDS listener
│   │       └── {sessions,messages,approvals,dto}.rs
│   └── installer/
│       ├── mod.rs           # ServiceInstaller trait, InstallContext, ServiceStatus,
│       │                    # for_current_platform, resolve_exec_start
│       ├── systemd.rs       # cfg(all(target_os = "linux",  feature = "linux"))
│       └── launchd.rs       # cfg(all(target_os = "macos",  feature = "macos"))
└── tests/
    ├── auth.rs                  # admin bearer auth
    ├── sse.rs                   # HttpAdapter fan-out
    ├── admin_has_no_channels.rs # /v1/sessions et al. are 404 on admin
    ├── openapi_spec_sync.rs     # asserts router spec == docs/openapi.json (UPDATE_OPENAPI=1 to rewrite)
    └── uds.rs                   # full UDS round-trip via hyper client
```

The frontend sources live outside the crate at `web/` (npm workspace,
not a Cargo member) and produce `web/dist/` which `build.rs` consumes.

The companion crate `aura-gateway-auth` exposes the PSK material, the
channel header constants (`TUI_PSK_HEADER`, `CHANNEL_TOKEN_HEADER`), and
the `ChannelTokenTable` type shared by both the gateway and the TUI
client.

## Collaboration

- **agent** — provides `SessionManager`, `JobManager`, `CronScheduler`,
  `MemoryManager`, `SecurityGateway`, `service::{ShutdownSignal,
  TaskTracker}` used by the server's graceful shutdown path.
- **channels** — the `ChannelAdapter`, `Message`, `IncomingMessage`,
  `OutgoingMessage`, `NoticeLevel`, and `ChannelRegistry` types the
  `HttpAdapter` implements and the POST route constructs.
- **gateway-auth** — embeds the build-time PSK, defines the
  `ChannelTokenTable` + `ClientIdentity` types, and owns the channel
  header constants. Shared between the gateway server and the TUI
  client so both agree on the wire protocol.
- **security** — `SecretVault` for admin-token persistence;
  `LeakDetector` for log redaction; `SecurityGateway` for the outgoing
  reveal path. The admin token, effective TUI PSK, and live channel
  tokens are all registered as `LeakDetector` rules at detector-
  construction time.
- **config** — `AuraConfig::gateway` drives admin bind address, channel
  socket path, CORS origins, and shutdown grace.
- **storage** — `Store::open` for vault bootstrap in `gateway_cmd`; the
  `TraceStore` trait behind `/v1/traces/:session_id`.
- **tui** — the TUI's `GatewayClient` connects over the channel UDS
  using the effective PSK; admin endpoints are not reached from the TUI.
- **cli** — `Commands::Gateway { cmd: GatewayCmd }` is defined in
  `crates/cli/src/cli.rs`; the dispatcher explicitly returns
  `UnknownCommand` because `src/main.rs` intercepts the variant before
  it reaches dispatch.
- **bootstrap** — `src/main.rs` intercepts `Commands::Gateway` and calls
  `gateway_cmd::run`; the long-running `start` path spins up both
  listeners against the same manager graph.

[`aura_channels::ChannelAdapter`]: ./channels.md
