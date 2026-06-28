# channels - Channel Ingress Layer

## Overview

The `channels` crate owns the in-process plumbing that pipes user messages into
the agent and fans agent output back to attached transports. The pure WS wire
types and MessagePack codec live in the separate `wire` crate; `channels`
re-exports it as `baybo_channels::wire` for existing server-side consumers.

There is **no adapter trait**. A [`Channel`] is a concrete struct — a
*protocol surface* (telegram, weixin, tui, http, …), 1:1 with
`ChannelType`. Each `Channel` owns its live [`Connection`]s (N per
channel, transport-provided) and, when its [`ChannelKind`] is
`Subscribed`, the reverse index from `session_id` to the connections
that asked to see it. The crate stays free of any wire-format or
transport details: the gateway provides a [`ConnectionSink`] impl that
wraps its outbound mpsc channels, and the fan-out logic in this crate
talks only to that sink.

Core responsibilities:

- Define the concrete [`Channel`] handle (`channel_type`,
  [`ChannelKind`], optional [`ApprovalSurface`]), the [`Connection`]
  transport instance, and the [`ConnectionSink`] trait the transport
  implements.
- Define the kind-typed [`SubscribedView`] that gates operations like
  `subscribe` / `unsubscribe` / `echo_inbound` /
  `broadcast_session_patch` / `broadcast_session_activity` /
  `set_dispatch_observer` on `Subscribed` channels — calling any of
  those on a `Multiplexed` channel is structurally unreachable.
- Define shared message types ([`Message`], [`IncomingMessage`],
  [`OutgoingMessage`], [`AgentOutput`], [`SessionEvent`],
  [`NoticeLevel`]).
- Define error types ([`ChannelError`], [`ConnectionNotFoundError`]).
- Provide [`ChannelRegistry`] for `Arc<Channel>` install / lookup and
  approval-gate fan-in.
- Re-export the `wire` crate under `wire::{Frame, Message}` (MessagePack, named
  fields) so the gateway's WS server, the built-in TUI's private WS client, the
  embedded web chat client, the iOS companion, and the third-party TypeScript SDK
  all speak the same protocol.
- Define the slash-command surface (`slash` module): the
  [`SlashHandler`] / [`DashboardProvider`] traits and their value
  types ([`SlashCommand`], [`SlashOutcome`], [`DashboardSnapshot`],
  [`ViewKind`]), plus the shared command constants (`COMPACT_COMMAND`
  / `COMPACT_COMMAND_NAME`, `STOP_COMMAND` / `STOP_COMMAND_NAME` /
  `STOP_COMMAND_DESCRIPTION`). The trait impls live outside the crate
  (e.g. `baybo-cli`) so channel adapters stay independent of the
  command layer while every adapter hooks into the same dispatcher.

## Channel and Connection

A `Channel` is a long-lived protocol surface; a [`Connection`] is one
live transport instance attached to it. The split lets the registry
stay a thin `ChannelType → Arc<Channel>` map and pushes per-WS
lifecycle (open / close / drop) into the channel itself.

```rust
pub struct Channel {
    channel_type: ChannelType,
    kind: ChannelKind,
    approvals: Option<ApprovalSurface>,
    connections: DashMap<ConnectionId, Arc<Connection>>,
    // Only consulted on Subscribed channels:
    subscriptions: DashMap<SessionId, DashSet<ConnectionId>>,
    // Pre-dispatch hook; installable only via SubscribedView.
    dispatch_observer: Mutex<Option<DispatchObserver>>,
}

impl Channel {
    pub fn new(channel_type: ChannelType, kind: ChannelKind, approvals: Option<ApprovalSurface>) -> Self;

    // Kind-agnostic dispatch surface — agent + tool path calls.
    pub fn dispatch_agent(&self, output: AgentOutput);
    pub fn dispatch_approval_requested(&self, call_id, session_id, user_id, tool, accesses, params_preview, description);
    pub fn dispatch_approval_resolved(&self, call_id, session_id, decision);

    // Transport lifecycle — gateway WS route calls these.
    pub fn attach(&self, conn: Arc<Connection>);
    pub fn detach(&self, id: ConnectionId);

    // Diagnostics.
    pub fn channel_type(&self) -> &ChannelType;
    pub fn kind(&self) -> ChannelKind;
    pub fn approval_gate(&self) -> Option<Arc<dyn ApprovalGate>>;
    pub fn has_subscribers(&self, session_id: &SessionId) -> bool;
    pub fn connection_count(&self) -> usize;
    pub fn pending_approvals(&self, session_id: &SessionId) -> Vec<ApprovalRequest>;
    pub fn pending_approval_call_ids(&self, session_id: &SessionId) -> Vec<String>;
    pub fn resolve_approval(&self, call_id: &str, decision: ApprovalDecision) -> Option<SessionId>;

    // Kind narrowing — see `SubscribedView` below.
    pub fn as_subscribed(&self) -> Option<SubscribedView<'_>>;
}
```

`Connection` is the per-WS handle the gateway hands to `Channel::attach`
after the `Register` handshake:

```rust
pub struct Connection { /* ConnectionId, Arc<dyn ConnectionSink>, subscribed: DashSet<SessionId> */ }
pub struct ConnectionId(pub Uuid);  // process-local, never serialised

pub trait ConnectionSink: Send + Sync + 'static {
    fn try_send_event(&self, event: SessionEvent) -> SendOutcome;
    fn try_send_frame(&self, frame: Frame) -> SendOutcome;
}
pub enum SendOutcome { Sent, Full, Closed }
```

Sinks are **synchronous and non-blocking**: the fan-out path cannot
afford to wait on a slow consumer. `Full` returns trigger a
`Frame::Reset` for that connection (nudging the client to refetch
history); `Closed` triggers `Channel::detach`.

## Channel Kinds

A channel's [`ChannelKind`] is fixed at construction (operators can't
toggle telegram-the-bot between modes) and decides how `dispatch_event`
fans output to connections:

| Kind          | Meaning                                                                                          | Used by                  |
| ------------- | ------------------------------------------------------------------------------------------------ | ------------------------ |
| `Multiplexed` | One connection carries every session of this `ChannelType`. Subscribe / Unsubscribe are protocol errors. | telegram, weixin, discord sidecars |
| `Subscribed`  | Connections receive only the sessions they explicitly subscribe to via `Subscribe` / `Unsubscribe` frames. | TUI (one subscription per process), http (the web chat page, switched on navigation) |

### Kind-typed `SubscribedView`

Operations that only make sense on a `Subscribed` channel —
`subscribe` / `unsubscribe` / `echo_inbound` /
`broadcast_session_patch` / `broadcast_session_activity` /
`set_dispatch_observer` — live on [`SubscribedView<'_>`], a cheap
borrow obtained from `Channel::as_subscribed()`. Multiplexed channels
return `None` from that call, so a caller holding only a `&Channel`
cannot accidentally invoke a Subscribed-only operation on a telegram-
shape channel. The previous runtime `WrongKind` error variant is
structurally unreachable and was removed; `SubscribedView::subscribe`
returns the narrow [`ConnectionNotFoundError`] for its one real
failure mode.

There is intentionally no symmetric `as_multiplexed()` — multiplexed
channels have no exclusive `Channel` operations today (bot control
flows through the gateway's `ChannelControlRegistry`, not `Channel`).
Add one when there's a real method to put on it.

## Dispatch Surface

External callers reach the channel through three kind-agnostic
methods on `Channel`:

| Method                          | Origin                                       | Event constructed              |
| ------------------------------- | -------------------------------------------- | ------------------------------ |
| `dispatch_agent(output)`        | Agent router (per-turn assistant output)     | `SessionEvent::Agent(_)`       |
| `dispatch_approval_requested(…)`| `ChannelApprovalGate` waker, when a tool call blocks | `SessionEvent::ApprovalRequested {..}` |
| `dispatch_approval_resolved(…)` | Connection sent `Frame::ResolveApproval` first; the resolver fans out so concurrent UIs drop the prompt | `SessionEvent::ApprovalResolved {..}` |

Plus one Subscribed-exclusive method on the view:

| Method                                | Used for                                                              |
| ------------------------------------- | --------------------------------------------------------------------- |
| `SubscribedView::echo_inbound(msg)`   | Echo a user message back to all tabs subscribed to its `session_id` so multi-tab views render through one path |

`SubscribedView` additionally exposes `broadcast_session_patch` /
`broadcast_session_activity` for sidebar-level frames that target all
attached connections regardless of subscription (sidebar freshness
doesn't need per-session keying — every tab maintains its full list).

All five funnel into the same internal `dispatch_event` /
`broadcast_frame` machinery: non-blocking, drops the frame for any
connection whose sink reports `Full` (and sends it a `Reset`), and
detaches connections whose sink reports `Closed`.

### Dispatch observer

Subscribed channels can carry a single pre-dispatch observer installed
via `SubscribedView::set_dispatch_observer`. It runs before fan-out
and receives `(&SessionEvent, SubscribedView<'_>)`, so it can re-enter
the typed broadcast path to emit a derived frame (today: the gateway's
`SessionPulse` watches every dispatch on the `http` channel and throttles
`Frame::SessionActivity` broadcasts for sidebar unread accounting).

## Event vs. Frame

```text
agent / tool path           channel / connection
        │                            │
   AgentOutput                       │
        │                            │
        ▼                            │
   SessionEvent  ── Channel::dispatch_event ─► ConnectionSink::try_send_event
                                                   (gateway translates
                                                    SessionEvent → wire::Frame)
                                                          │
                                                          ▼
                                                       wire bytes
```

[`AgentOutput`] is the narrow set of things the agent itself emits
(`AnswerDelta`, `Reasoning`, `ToolStarted`/`ToolCompleted`, `Message`,
`Notice`). [`SessionEvent`] wraps that plus the
channel-side events (`UserEcho`, `ApprovalRequested`, `ApprovalResolved`)
so the agent's output surface stays statically narrow. The gateway's
per-connection translator converts each `SessionEvent` to the matching
[`wire::Frame`] before serialisation. Wire-format knowledge stays in
the gateway crate.

## Channel Registry

`ChannelRegistry` is a thin map from `ChannelType` to `Arc<Channel>`
plus a shared `ApprovalGateMap` populated at install time. Channels
are eagerly created at gateway boot from `ChannelsConfig` and installed
once; nothing here drops them at runtime.

```rust
pub struct ChannelRegistry { /* DashMap<ChannelType, Arc<Channel>>, Arc<ApprovalGateMap> */ }

impl ChannelRegistry {
    pub fn new() -> Self;
    pub fn approval_gates(&self) -> Arc<ApprovalGateMap>;
    pub fn install(&self, channel: Arc<Channel>) -> Result<()>;       // boot-time only
    pub fn get(&self, channel_type: &ChannelType) -> Option<Arc<Channel>>;
    pub fn list(&self) -> Vec<ChannelType>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

`install` errors with `ChannelError::DuplicateChannel` if the type is
already installed — that's a config bug (operator declared the same
channel twice). There is no `unregister`: channels are pinned for the
lifetime of the gateway process. The previous `Session-scoped clients`
split (one `Arc<Channel>` per `session_id` for the TUI) and the
`DuplicateSessionClient` error are gone — the TUI is now a `Subscribed`
connection on the `tui` channel, keyed by `ConnectionId` like any other
client.

Bootstrap hands the shared `Arc<ApprovalGateMap>` to `ToolExecutor` so
gates registered later are visible immediately without re-plumbing. The
gate map is per-channel-type only (the registry no longer splits
per-`(channel, session)`); concurrent TUI / web tabs subscribed to the
same session resolve approvals through the same gate, and the channel
fans out `ApprovalResolved` so the loser tabs drop their prompt card.

## Channel Implementations

Today the only in-tree transport is the gateway's `/v1/channel-ws`
server (`crates/gateway/src/channel/`). Every channel — the built-in
TUI, the embedded web chat page, and every out-of-process sidecar —
reaches the agent through that same endpoint:

| Channel        | Kind          | ID prefix | Transport / auth                                                            |
| -------------- | ------------- | --------- | --------------------------------------------------------------------------- |
| `tui`          | `Subscribed`  | `tui_`    | `/v1/channel-ws` (per-start token from `gateway.tui_token`)                 |
| `http` (web)   | `Subscribed`  | `web_`    | `/v1/channel-ws` (per-tab token minted by `POST /v1/chat/sessions/:id/token`) |
| Sidecars       | `Multiplexed` | `<name>_` | `/v1/channel-ws` (per-spawn capability token, claims its own `channel_type`)|

See [`tui.md`](./tui.md) for the TUI client side and
[`gateway.md`](./gateway.md) for the server side (including the
http-channel token lifecycle and the `StashedTokenHandle` /
`web_chat_tokens` TTL janitor that binds a web token's lifetime to its
WS).

The only public SDK for third-party sidecars is the TypeScript package
at `sdks/channel-ts/`. Its primary surface is a `Channel` interface
plus `runChannel(channel)` entry point: the sidecar author implements
`onMessage`, `inbound(signal)`, and optionally `onApprovalRequested`
/ `onDelta` / `onNotice`, and the SDK handles the WebSocket +
MessagePack transport, `Register`/`Ack` handshake, loopback-TCP dial,
frame dispatch, concurrent approval spawning, and auto-reconnect with
exponential backoff + jitter on transient drops (disable with
`reconnect: false`).

The SDK-provided default logger emits one NDJSON record per call
(`{"level": "...", "msg": "..."}`) to `process.stdout` (debug/info) or
`process.stderr` (warn/error). When the sidecar runs under
[`SidecarSupervisor`] (the in-tree path), the gateway's per-channel
pipe drain parses each line back into structured records and pushes
them into the same `LogBuffer` that backs `/v1/logs` plus the
per-channel file at `<channel_log_dir>/<channel_type>.log.<date>` —
single write path, no parallel WS sink. A non-JSON line (third-party
logger, console.log) falls back to a plain text record at info
(stdout) or warn (stderr). Attribution uses
`sidecar::<channel_type>[::<target>]`.

The channel WebSocket URL + token are read from `BAYBO_CHANNEL_URL` /
`BAYBO_CHANNEL_TOKEN` env vars. `ChannelSpawner`
(`crates/gateway/src/spawn.rs`) is the primitive that mints a fresh
token per spawn and injects both env vars; the URL the supervisor
builds is `ws://127.0.0.1:<port>/v1/channel-ws` where `<port>` comes
from [`ChannelServer::port`] after it binds on `127.0.0.1:0`.
[`SidecarSupervisor`] (`crates/gateway/src/sidecar/supervisor.rs`)
drives it: at gateway boot, [`SidecarRuntime::install`] materialises
each sidecar's JS bundle to
`$XDG_CACHE_HOME/baybo/sidecars/<channel>-<hash>/bundle.mjs` (plus any
declared aux assets next to it), then one supervised task per embedded
channel type runs `Command::new("bun").arg(bundle).spawn()` through
`ChannelSpawner` and restarts on exit with exponential backoff
(500ms → 30s, reset after ≥60s of stable uptime). The `bun` executable
is resolved from `PATH`; set `BAYBO_BUN_BIN` to point at a specific
install. Shutdown fans out via the shared `ShutdownSignal` — children
are SIGKILLed and awaited. Bringing up a custom sidecar out-of-tree is
still possible: export the two env vars yourself (or pass `wsUrl` /
`token` to `runChannel`) and run the process however you like.

**Out-of-tree sidecars and observability.** A sidecar started by an
external process manager (systemd, docker, foreman, …) speaks the
same `/v1/channel-ws` protocol — the WS frame stream, blob HTTP
side-channel, and `Register`/`Ack` handshake all work unchanged — but
the gateway is **not** the parent process and therefore does not drain
its stdout/stderr. The default logger's NDJSON records land wherever
that process manager captures them (`journalctl`, `docker logs`, the
wrapping shell), **not** in `LogBuffer` or the per-channel file. The
gateway's `/v1/logs` view will show only gateway-internal tracing for
that channel; the sidecar's runtime errors are visible only through
whatever sink the operator configured for the child process. This is
an intentional simplification of the previous design — a parallel
`Frame::SidecarLog` sink mirrored every log line over the WS so
out-of-tree sidecars also surfaced in `LogBuffer`, but it created two
write paths that drifted (one bug let the file logger silently miss
every post-handshake line) and added a synchronisation invariant
(`if sink != null skip console`) that was easy to violate. If you need
gateway-side log visibility for an externally managed sidecar today,
route its stdout/stderr into the gateway by adopting one of the
supervisor patterns: run it as a child of the gateway via
[`SidecarSupervisor`], or pipe its stdout into a small forwarder that
connects to `/v1/logs`'s admin endpoint. There is no SDK-level shortcut.

**Embedded sidecar toolchain.** `crates/gateway/build.rs` runs
`pnpm --filter <pkg> bundle` for each `channel-src/*` package; the
`bundle` script in each `package.json` invokes
`bun build --target=bun --minify` to emit a single self-contained
`dist/bundle.mjs`. The output, plus any `baybo.auxAssets` declared in
the package (e.g. weixin's `silk.wasm`), is zstd-compressed into
`target/sidecar-cache/<fingerprint>/` and embedded via
`include_bytes!`. Sidecar packaging is keyed by the relevant sidecar
inputs (workspace lockfiles, `channel-src/*` sources/configs, and
`sdks/channel-ts/dist`): if those inputs are unchanged, a later
`cargo build` reuses the cached compressed bundles instead of
re-running bun. `--target=bun` substitutes bun's own polyfills for
`ws` (WHATWG `WebSocket`) and `node-fetch` (bun's native fetch); the
channel SDK is careful to stay within WHATWG `WebSocket` (auth token
rides in a `?token=…` query-string rather than a custom HTTP header)
so the same bundle would also run on node, but bun is what we ship
against because the bun-substituted output dodges several runtime
landmines in node-fetch@2 / whatwg-url@5 / dual-package CJS deps.
Failures (missing `node_modules`, bun bundle error, missing `pnpm`)
degrade to `cargo:warning=…` + empty assets — `cargo build` still
succeeds, the supervisor just logs "embedded sidecar runtime
unavailable" and skips the spawn loop. Set `BAYBO_REQUIRE_SIDECARS=1`
to flip those degrades into hard build failures for release CI.

Raw wire types are re-exported under the `./wire` subpath for advanced
callers. There is no Rust SDK — the TUI has its own private WS client,
and the server is authoritative on the wire format.

The first in-tree sidecar built on the SDK is the Telegram channel at
`channel-src/telegram/` (package `@baybo/channel-telegram`). It uses
`grammy` for long-polling, maps Telegram `chat_id`s to stable UUIDv5
`session_id`s, and surfaces `Frame::ApprovalRequested` as an inline-
keyboard prompt in the originating chat. It's also the working example
of the full `Channel` contract — `inbound(signal)` pump, `onMessage` /
`onNotice` round-trip, concurrent `onApprovalRequested`, `onStop`
cleanup.

## Bot Registration

`baybo channel add` is a separate, one-shot mode of the same bundle
that runs the channel at runtime. The CLI spawns
`bun <bundle>` with `BAYBO_CHANNEL_MODE=register` set; the SDK's
[`runSidecar`] notices the env var and dispatches into the channel's
optional `register(ctx)` hook instead of the normal `runChannel` path.
There is no WebSocket and no capability token in this mode — the
subprocess is locally driven over stdin/stdout and never connects back
to the gateway.

**Wire**: line-delimited JSON, types defined under
`crates/channels/src/register_wire.rs` and exported to TS via `ts-rs`.
Sidecar→host frames: `Prompt { id, label, kind: input|password, required }`,
`Result { bot_id, token }`, `Error { message }`. Host→sidecar frames:
`PromptReply { id, value }`, `Cancel`. **stdout is protocol-only**;
human-facing output (QR codes, progress text) goes to stderr, which the
CLI inherits. Sidecar→host frames are capped at ~1 MiB (sized to fit a
1 MiB `token` plus envelope overhead); the SDK separately caps
host→sidecar frames at 64 KiB since prompt replies are user input.

**SDK contract**: declare a `register` function on `runSidecar`'s
options. The function receives a `RegistrationContext { input,
password }` (each prompt awaits the host's reply over the wire) and
returns `{ botId, token }`. Throw to surface a registration failure as
an `Error` frame. Multiple separate credentials should be packed into
the single `token` string as JSON (the weixin sidecar already uses this
pattern via its `AuthBlob`).

```ts
// channel-src/telegram/src/index.ts (excerpt)
register: async (ctx) => {
  const token = await ctx.password("bot token: ", { required: true });
  const colon = token.indexOf(":");
  if (colon <= 0 || !/^\d+$/.test(token.slice(0, colon))) {
    throw new Error("invalid telegram token");
  }
  return { botId: token.slice(0, colon), token };
}
```

**CLI driver**: `crates/setup/src/flow/channel/register_driver.rs`
(`run_registration`) enforces the contract from the host side. It runs the bundle with
`Command::env_clear()` + a small allowlist (`PATH`, `HOME`, `TERM`,
`LANG`, `LC_*`, `TZ`, `TMPDIR`, plus `BAYBO_CHANNEL_MODE=register`) so
no `BAYBO_*` value (capability tokens, vault endpoints, gateway URL)
leaks in. The driver enforces a 10-minute overall timeout, a 5-second
post-result exit grace, and `kill_on_drop(true)` so any error path
terminates the subprocess. On success it persists exactly one vault
key, `channel.<channel_type>.bot.<bot_id>.token`, mirroring what
`Frame::StartBot` reads at runtime — the sidecar receives the same
string back via `onStartBot(cmd)` when the gateway later boots the
bot.

**Reference implementations**: `channel-src/telegram/src/index.ts`
(single-prompt token validation) and `channel-src/weixin/src/cli.ts`
(non-interactive QR scan with progress on stderr) are the two working
examples.

## Error Strategy

- Connection failures are the transport's problem. The gateway
  unregisters by dropping the `Arc<Connection>` from the channel
  (`Channel::detach`) when the WS closes; the channel's reverse index
  is cleaned up in the same call.
- Outbound failures are non-retrying: `SendOutcome::Full` triggers a
  `Frame::Reset` to nudge the client back to a known state via REST
  history refetch (transcript is the canonical record; deltas / notices
  / approval prompts are advisory). `SendOutcome::Closed` triggers
  `Channel::detach` on the offending connection.

## Unified Message Mapping

All platforms map to the same [`IncomingMessage`] structure via
consistent ID prefixing (e.g. `tg_{msg_id}`, `dc_{msg_id}`,
`web_{uuid}`) and session derivation rules. Session ids are
client-generated UUIDs; the router resolves or creates the session on
first message via `SessionManager::get_or_create`.

`IncomingMessage.platform_msg_id` is the client-supplied idempotency
key carried over from `wire::Message.platform_msg_id`. It's echoed
back unchanged in the `SessionEvent::UserEcho` fan-out so the
originating tab can reconcile its optimistic placeholder against the
authoritative server row instead of double-rendering. Empty when the
carrier didn't set one (older bundles, TUI, fixtures).

## Constraints

- `channels` stays independent of `agent`, `llm`, and other business
  crates (depends only on `baybo-model` and `baybo-tools`; `baybo-tools`
  is pulled in only for the `ApprovalGate` + `ApprovalGateMap` types).
- Transports own framing and encoding. This crate only defines the
  neutral `wire::{Frame, Message}` shapes (MessagePack-named) and the
  `encode` / `decode` helpers both sides call; the gateway route and
  the TUI's private WS client each own their own WebSocket plumbing.

## Collaboration

| Module     | Role                                                                                                              |
| ---------- | ----------------------------------------------------------------------------------------------------------------- |
| `model`    | Provides `ContentBlock`, `ChannelType`, `SessionId`, `ResourceAccess`, `User`                      |
| `agent`    | Router owns the registry and calls `Channel::dispatch_agent` (and the approval-dispatch helpers) by `ChannelType` |
| `tools`    | Provides `ApprovalGate` + `ApprovalGateMap` reused by the registry; `ApprovalQueue` backs `ApprovalSurface`       |
| `gateway`  | Hosts the only in-tree transport (`/v1/channel-ws`); installs channels at boot, builds per-WS `Connection`s, owns the `ConnectionSink` impl, and translates `SessionEvent` → `wire::Frame` |
| `security` | Inbound user messages go to `SecurityGateway` first after entering the system                                     |
