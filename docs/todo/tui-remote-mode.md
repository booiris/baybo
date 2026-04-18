# TUI as Gateway Client (remote-only)

## Problem

Today a workspace supports exactly one `aura` process:
`singleton::acquire` (`src/singleton.rs:25`) takes an advisory `flock`
on `<workspace>/aura.lock`, and both `tui_cmd::run`
(`src/tui_cmd.rs:39`) and `gateway_cmd::start`
(`src/gateway_cmd.rs:302`) acquire it before booting. The lock is
deliberate: each path constructs the full manager graph via
`runtime::build_managers`, which owns the libsql store, cron tick
loop, job-recovery scan, and tool executor. Two processes against the
same workspace would race on every one of those.

Practical consequence: if a user runs `aura gateway start` as a system
service, they can't also open `aura tui` against the same workspace
— the TUI fails with "another aura instance is running". Carrying two
parallel bootstraps (TUI builds its own graph; gateway builds its own
graph) also keeps a lot of duplicated wiring between `src/tui_cmd.rs`
and `src/gateway_cmd.rs` that `runtime::build_managers` only partly
compresses.

The gateway already exposes the full manager surface over HTTP
(`docs/modules/gateway.md`): sessions, messages (SSE), jobs, cron,
memory, traces, skills, tools, channels, llm, config, status. The
cleaner story is to make the gateway the **only** long-running
process and turn the TUI into a thin HTTP+SSE client — one backend,
many frontends.

## Proposed Direction

Make `aura tui` **remote-only**. No more local manager graph, no more
singleton lock from the TUI path, no more local Router. The gateway
becomes a hard prerequisite.

### Baseline behavior

- On `aura tui` startup, resolve the gateway endpoint:
  - `--gateway <url>` flag (per-invocation override), else
  - `AURA_GATEWAY_URL`, else
  - `<config>.gateway.bind_address`/`port`.
- Resolve the auth token:
  - `--token`, else `AURA_GATEWAY_TOKEN`, else the shared vault
    (`gateway.auth_token` via `runtime::build_secret_vault` — cheap,
    reads only; does not need the singleton lock).
- Probe `GET /healthz`. If it fails, exit with a concrete error that
  tells the user what to do next:

  ```
  error: no aura gateway reachable at http://127.0.0.1:8890
    - start it with:       aura gateway start
    - or install service:  aura gateway install && aura gateway enable
    - (dev only) retry with --dev-auto-gateway to spawn one inline
  ```

- On success, attach a `GatewayClient` and drive the ratatui UI against
  it.

There is no longer a "local" TUI mode. `src/tui_cmd.rs` does not
import `runtime::build_managers` — the whole boot path collapses to
"open vault → read token → HTTP client → ratatui loop".

### Dev-only auto-start

Behind a compile-time gate (`cfg(debug_assertions)` plus an opt-in
cargo feature `dev-gateway-autostart` so release builds cannot enable
it even with `RUSTFLAGS`), add `--dev-auto-gateway` to `aura tui`.
When set and no gateway is reachable, the TUI boots one before
connecting. Two viable shapes — pick during implementation:

1. **In-process** — spawn `GatewayServer::run` as a tokio task in the
   same process, then connect to `127.0.0.1:<port>` as usual. The
   gateway task holds the workspace singleton; the TUI side never
   acquires it. Benefit: single process, single log stream, no
   subprocess lifetime management. Cost: tracing init has to be
   reconciled (the TUI needs `TuiLogLayer`; the gateway wants a file
   subscriber — can't `init()` twice). Probably means the dev path
   installs the TUI's layered subscriber once and the gateway task
   reuses it.

2. **Subprocess** — `Command::new(std::env::current_exe()).arg("gateway")
.arg("start")` as a child, wait for its `/healthz` to come up, then
   connect. Benefit: clean process isolation, each side keeps its own
   tracing setup. Cost: parent has to reap the child on exit, stream
   its stderr somewhere useful, and handle the race where the child
   binds slowly.

Either way the dev flag:

- Prints a loud `DEV: auto-started gateway for TUI — not for
production` banner.
- Is gone from `--help` output in release builds.
- Has a unit test that `cargo build --release` rejects it.

### What has to change

- **`src/tui_cmd.rs`** — gutted. No singleton, no `build_managers`,
  no `wire_router`, no `TuiAdapter` registration into a local
  `ChannelRegistry`. Just: resolve endpoint + token → client →
  ratatui.
- **`src/runtime.rs`** — `build_managers` + `wire_router` are now
  only called from `gateway_cmd::start`. Can drop the
  `shutdown: #[allow(dead_code)]` on `ManagerGraph` since it's no
  longer kept for the TUI's "future use". Consider folding the
  helpers back into `gateway_cmd` if no other caller appears, or
  keep `runtime.rs` as the single assembly point and delete nothing.
- **New `GatewayClient`** — likely in `crates/gateway/src/client.rs`
  or a new `crates/gateway-client` crate. Wraps reqwest + SSE;
  exposes methods mirroring the HTTP surface.
- **`TuiAdapter` transport** (`crates/channels/src/tui.rs`) — today
  the adapter sends `IncomingMessage` onto a local Router's
  `incoming_tx`. In the remote world it POSTs to
  `/v1/sessions/:id/messages` and subscribes to
  `/v1/sessions/:id/stream` for delta/response/notice events. Either
  pluggable transport or a sibling `RemoteTuiAdapter` sharing the
  ratatui UI layer.
- **`CliSlashHandler` / `CliDashboardProvider`**
  (`crates/cli/src/lib.rs`) — both pull from live manager `Arc`s
  today. They need a "data source" abstraction so the slash commands
  and dashboard drive `GatewayClient` instead. Several slash commands
  (`/memory add`, `/cron new`) are already HTTP-shaped; others
  (`/config set`) mutate the vault directly and need either
  server-side endpoints or explicit "unavailable in TUI" errors.
- **Approval gates** — `HttpAdapter::approval_gate()` returns `None`
  today; tool calls that require approval auto-deny. Remote TUI is
  the first real caller that wants the prompt surfaced to a human,
  so the gateway needs `GET /v1/approvals`, `POST /v1/approvals/:id`,
  and an `approval_needed` SSE event. Blocks remote TUI being usable
  for anything beyond read-only chat.
- **Input history** — `CliInputHistoryStore` writes directly to the
  vault. Keep that path (vault reads/writes don't take the singleton
  lock), so per-host history survives without a new endpoint.

### Decisions to resolve during implementation

- Which slash commands are allowed? Fan-out to new endpoints?
  Read-only subset? Explicit errors for the rest?
- Aggregated `/v1/dashboard` endpoint vs client-side fan-out (latency
  vs duplicated shape logic).
- SSE reconnection: replay missed deltas from `trace` or show a gap
  marker?
- Dev auto-start in-process vs subprocess — decide after a spike on
  tracing-subscriber reuse.
- Rollout: is there a transition release where `aura tui` without a
  gateway still works, or is the cut a single breaking change?

## Related

- `src/singleton.rs` — the lock that motivated the split
- `src/tui_cmd.rs`, `src/gateway_cmd.rs:297` — current dual bootstraps
- `crates/gateway/src/http_adapter.rs` — server-side SSE stream the
  client needs to speak
- `crates/cli/src/lib.rs` — `CliSlashHandler` and
  `CliDashboardProvider`, today coupled to local managers
- `docs/modules/gateway.md` — current gateway API surface + the open
  approval-gate note that must close before remote TUI is usable
- `docs/modules/tui.md` — will need a rewrite to reflect remote-only
  semantics
