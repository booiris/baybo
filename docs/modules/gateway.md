# gateway - HTTP Server and Service Installer

## Overview

`aura-gateway` is Aura's headless backend. One axum server plays two roles:

1. **Chat transport** — it implements [`aura_channels::ChannelAdapter`] for
   `ChannelType::Http` so chat traffic flows through the exact same Router
   path used by the TUI. No parallel agent pipeline.
2. **Admin REST + SSE API** — it exposes the operator surface (sessions,
   messages, jobs, cron, memory, traces, skills, tools, channels, llm,
   config, status) over HTTP, mirroring the CLI command families.

The gateway is driven by the `aura gateway …` command tree. `start` runs
the server in the foreground; `install` / `enable` / `disable` /
`uninstall` / `status` manage the platform service unit; `token
{show|rotate}` manages the dynamic auth token. The binary entrypoint
intercepts `Commands::Gateway` in `src/main.rs` before the CLI dispatcher
and routes it to `src/gateway_cmd.rs` — same pattern as `Commands::Tui`.

Configuration lives in `aura_config::GatewayConfig` (a new section on
`AuraConfig`). The stub `HttpChannelConfig` at
`crates/config/src/channels.rs:60` is kept for one release for backwards
compatibility but is no longer read; the gateway owns its own settings.

## Design Decisions

### Single server, two surfaces

The gateway fills the existing `HttpChannelConfig` stub rather than
running as a detached worker. Doing so keeps the adapter contract
(`send_response`, `send_stream_delta`, `send_notice`, `approval_gate`)
honest: any message the LLM emits over the HTTP channel goes through
the same `ChannelRegistry` dispatch as TUI/telegram/discord, and the
router picks the outgoing adapter by `message.channel`
(`crates/agent/src/router.rs:417`). Mis-tagging the channel would route
responses to the wrong adapter, which is why `POST
/v1/sessions/:id/messages` always rebuilds the `User` with
`ChannelType::Http` before submission.

### Dynamic per-install token, stored in `SecretVault`

There is no shared-secret or config-file token. On `aura gateway enable`,
`GatewayToken::mint_if_absent` either reads the current token or
generates a fresh 32-byte random value (hex-encoded) and writes it under
the vault key `gateway.auth_token` — the same AES-256-GCM store the rest
of Aura uses. `token show` reads, `token rotate` overwrites. `uninstall`
removes the service unit but leaves the token in the vault, so re-
installing on the same workspace keeps working; `token rotate` is the
explicit way to invalidate a leaked token. Because the vault requires
the master encryption key,
the token's confidentiality rides on the same root secret as every other
credential in the project.

### Constant-time compare, URI sanitisation before tracing

The auth middleware (`crates/gateway/src/auth.rs`) accepts
`Authorization: Bearer <token>` first and falls back to `?token=<token>`
for embedded links (SSE streams opened from a browser, quick-copy
`curl`s). Comparisons run through a small explicit `constant_time_eq`
helper — no `subtle` dependency, but the loop never short-circuits on
mismatch length. After a successful compare, the middleware **strips
`?token=` from the request URI in place** so `tower_http::trace::TraceLayer`
never sees the token in structured logs. `/healthz` and `/readyz` skip
the middleware entirely.

### `LeakDetector` rule is attached at construction, not at mint

`LeakDetector::add_rule` takes `&mut self`; the detector is shared as
`Arc<LeakDetector>` across every manager once the runtime is wired, so
there is no way to register a new redaction rule after construction.
The workable option is to **read the vault-stored token in
`runtime::build_managers` before the detector is built** and add it as a
rule on the fresh detector. If the vault has not been seeded yet (first
`start` before `enable`), the runtime fails fast with a message telling
the user to run `aura gateway enable` first. This means `enable` needs
the full storage init path (open `Store`, open `SecretVault`), not just
a one-shot file write.

### Adapter owns SSE fan-out

`HttpAdapter` keeps a `RwLock<HashMap<SessionId, broadcast::Sender<SseEvent>>>`.
`subscribe` returns a receiver (creating the broadcast channel on
first use); `submit` pushes an `IncomingMessage` into the router's
`incoming_tx` captured during `ChannelAdapter::start`. Outgoing hooks
(`send_response`, `send_stream_delta`, `send_notice`) fan out to the
matching sender via `broadcast::Sender::send`, which is lossy only when
a subscriber is lagging past the buffer — in that case
`BroadcastStream` surfaces `Lagged` to the SSE handler, which emits an
`error` event and continues. `stop` clears the map, which drops all
senders and signals EOF on every live stream.

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

### Gateway owns its own DTOs

Route handlers call the manager `pub async fn` methods directly
(`SessionManager`, `JobManager`, `CronScheduler`, `MemoryManager`,
`TraceStore`, `SkillRegistry`, `ToolRegistry`) and serialise into DTOs
defined in `crates/gateway/src/api/dto.rs`. CLI handler output is not
reused — those are built around `CommandContext` + `OutputFormat` and
would force a cross-crate refactor of every `crates/cli/src/commands/`
module. The duplicated surface is small and the independence is worth
more than the line count saved.

### Platform installers behind Cargo features

```
default = ["linux", "macos"]
linux        = []   # systemd installer — renders ~/.config/systemd/user/aura-gateway.service
macos        = []   # launchd installer — renders ~/Library/LaunchAgents/com.aura.gateway.plist
test-support = []   # cross-crate test helpers (CLAUDE.md gating rule)
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

Mounted under `/v1`. Every authenticated route goes through
`require_token` middleware.

```
GET    /healthz                         liveness (no auth)
GET    /readyz                          managers ready (no auth)
GET    /v1/status                       aura status mirror
GET    /v1/config                       redacted config snapshot (read-only)
PUT    /v1/config                       { path, value } → 200 { path, written_to, requires_restart }
DELETE /v1/config                       { path } → 200 { path, written_to, requires_restart }

GET    /v1/sessions
POST   /v1/sessions                     { user_id?, name? }
GET    /v1/sessions/:id
DELETE /v1/sessions/:id                 soft-delete
GET    /v1/sessions/:id/messages        history

POST   /v1/sessions/:id/messages        { text } → 202 { message_id }
GET    /v1/sessions/:id/stream          SSE: delta | response | notice | end

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
GET    /v1/channels
GET    /v1/llm

GET    /v1/approvals                    list pending approvals
POST   /v1/approvals/:call_id           { decision: "approve" | "deny" } → 200 { call_id, decision }
GET    /v1/approvals/stream             SSE: added | resolved | end
```

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
`wire_router` — the TUI talks to the gateway over HTTP and holds no
manager graph of its own. The gateway registers the `HttpAdapter` into
`graph.channels_registry`, builds a `GatewayServer` from
`GatewayDeps`, and drives the axum server in parallel with the router
via `tokio::select!` on `shutdown.wait`.

`build_secret_vault` is a narrow helper that only opens the libsql
store far enough to construct a `SecretVault`. The TUI's remote boot
uses it to read the gateway token without touching the workspace
singleton lock; the vault-only `gateway token {show,rotate}`
subcommands use it for the same reason.

## Security and Observability

- **Token never logged unredacted.** Two layers: (a) the `LeakDetector`
  rule registered at detector construction redacts any log line that
  happens to echo the token; (b) the auth middleware strips `?token=`
  from the URI before the `TraceLayer` span is emitted.
- **All gateway logs pass through `RedactingMakeWriter`** — the same
  writer wrapper the TUI uses (`src/logging.rs`). `AURA_LOG_FORMAT=json`
  is honoured.
- **Every mutation goes through a manager.** Route handlers call
  `SessionManager`, `JobManager`, `CronScheduler`, `MemoryManager` and
  friends directly; there are no side-channel writes that bypass
  Trace/Job observability.
- **Singleton lock applies only to the gateway.** `start` acquires
  the per-workspace lock on `<workspace>/aura.lock`. `aura tui` is an
  HTTP+SSE client (see [`tui.md`](./tui.md)) and **does not** take
  the lock, so one long-lived `aura gateway` can serve many
  concurrent TUI sessions in the same workspace.

## Crate Layout

```
crates/gateway/
├── Cargo.toml               # features above, all deps via workspace = true
├── src/
│   ├── lib.rs               # re-exports GatewayServer, GatewayDeps, GatewayToken, …
│   ├── config.rs            # RuntimeGatewayConfig (resolves SocketAddr + shutdown grace)
│   ├── server.rs            # GatewayDeps, ApiState, GatewayServer
│   ├── http_adapter.rs      # HttpAdapter : ChannelAdapter + SseEvent
│   ├── auth.rs              # GatewayToken, AuthState, require_token, constant_time_eq
│   ├── error.rs             # GatewayError : IntoResponse
│   ├── api/
│   │   ├── mod.rs           # v1_router()
│   │   ├── dto.rs           # request/response DTOs
│   │   ├── {sessions,messages,jobs,cron,memory,traces,skills,tools,channels,llm,config,status,health}.rs
│   └── installer/
│       ├── mod.rs           # ServiceInstaller trait, InstallContext, ServiceStatus,
│       │                    # for_current_platform, resolve_exec_start
│       ├── systemd.rs       # cfg(all(target_os = "linux",  feature = "linux"))
│       └── launchd.rs       # cfg(all(target_os = "macos",  feature = "macos"))
└── tests/
    └── …                    # router assembly, auth middleware, SSE fan-out, installer snapshots
```

## Collaboration

- **agent** — provides `SessionManager`, `JobManager`, `CronScheduler`,
  `MemoryManager`, `SecurityGateway`, `service::{ShutdownSignal,
  TaskTracker}` used by the server's graceful shutdown path.
- **channels** — the `ChannelAdapter`, `Message`, `IncomingMessage`,
  `OutgoingMessage`, `NoticeLevel`, and `ChannelRegistry` types the
  `HttpAdapter` implements and the POST route constructs.
- **security** — `SecretVault` for token persistence; `LeakDetector` for
  log redaction; `SecurityGateway` for the outgoing reveal path when
  outbound handlers materialise credentials. The token is registered as
  a `LeakDetector` rule at detector-construction time.
- **config** — `AuraConfig::gateway` (new) drives bind address, port,
  CORS origins, and shutdown grace.
- **storage** — `Store::open` for vault bootstrap in `gateway_cmd`; the
  `TraceStore` trait behind `/v1/traces/:session_id`.
- **cli** — `Commands::Gateway { cmd: GatewayCmd }` is defined in
  `crates/cli/src/cli.rs`; the dispatcher explicitly returns
  `UnknownCommand` because `src/main.rs` intercepts the variant before
  it reaches dispatch.
- **bootstrap** — `src/main.rs` intercepts `Commands::Gateway` and calls
  `gateway_cmd::run`; the long-running `start` path is pending the
  `runtime::` module extraction.

[`aura_channels::ChannelAdapter`]: ./channels.md
