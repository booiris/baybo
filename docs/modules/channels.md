# channels - Channel Ingress Layer

## Overview

The `channels` crate defines the shared wire contract for everything
that pipes user messages into the agent and agent output back out. It
exposes a concrete [`Channel`] handle plus a [`ChannelRegistry`] the
agent uses for per-`ChannelType` dispatch. There is **no trait** —
transports (today: the gateway's `/v1/channel-ws` sidecar pump) build a
`Channel`, register it, and own the far end of its outbound mpsc.

Core responsibilities of this crate:

- Define the concrete [`Channel`] handle (`channel_type`, outbound
  `mpsc::Sender<AgentOutput>`, optional approval gate)
- Define shared message types (`Message`, `IncomingMessage`,
  `OutgoingMessage`, `AgentOutput`, `NoticeLevel`)
- Define error types (`ChannelError`)
- Provide [`ChannelRegistry`] for `Arc<Channel>` registration, lookup,
  and approval-gate fan-in
- Expose the sidecar wire format under `wire::{Frame, Message}`
  (MessagePack, named fields) so the gateway's WS server, the built-in
  TUI's private WS client, and the third-party TypeScript SDK all
  speak the same protocol. `Frame` includes `HistorySnapshot` /
  `HistoryAppend` variants used exclusively by session-scoped TUI
  clients to load and persist their input ring over the same WS the
  chat frames ride on (see [`gateway.md`](./gateway.md) and
  [`tui.md`](./tui.md)); sidecars ignore them.

## Design Decisions

### No adapter trait

`ChannelAdapter` used to be a trait; every transport implemented
`send`, `start`, `stop`, and `approval_gate`. Collapsing the trait to
a concrete `Channel` struct with a single [`Channel::send`] forwarding
over a `mpsc::Sender<AgentOutput>` made the router free of dynamic
dispatch and removed an entire lifecycle axis (start/stop) that the
WS transport was already duplicating. Wire-format knowledge lives in
the transport crate (the gateway), not here.

### Transports own lifecycle

`ChannelRegistry::register` and `unregister` are the only lifecycle
hooks. There is no `start_all` / `stop_all`: each transport registers
when its connection comes up (e.g. the gateway route task after a
successful WS handshake) and unregisters when it drops. The registry
only tracks live handles.

### Single outbound entrypoint

All outbound traffic goes through one method: `Channel::send(output:
AgentOutput) -> Result<()>` forwards onto the outbound mpsc. The
transport decides how to serialise each variant (`Delta`, `Message`,
`Notice`) onto the wire. The router just looks up the channel by
`ChannelType` and pushes events through.

### Streaming vs. final output

`AgentOutput::Delta` carries incremental text chunks;
`AgentOutput::Message` carries the final, canonical response.
Transports that can render partial output — the TUI — accumulate deltas
as they arrive and reconcile against the `Message` when the turn
finishes. Transports that only support one-shot delivery may drop
deltas in their frame encoder and act only on `Message`. Delivery
ordering per `session_id` is the caller's responsibility; adapters
assume chunks arrive in the order the LLM emitted them.

### Out-of-band notices

`AgentOutput::Notice { level: NoticeLevel, text, … }` is the path for
events the user didn't prompt for but should see — e.g. "a skill you
invoked was rated suspicious and was kept with a warning". Transports
without a side-channel for this may drop it. The gateway WS pump
forwards each `Notice` as a `Frame::Notice` with a lower-case
`"warn"` / `"error"` level string so third-party SDKs don't need a
typed enum to render it.

### Unified message mapping

All platforms map to the same `IncomingMessage` structure via
consistent ID prefixing (e.g. `tg_{msg_id}`, `dc_{msg_id}`) and session
derivation rules. Session ids are client-generated UUIDs; the router
resolves or creates the session on first message via
`SessionManager::get_or_create`.

### Error handling strategy

- Connection failures are the transport's problem. The channel is
  unregistered when the connection drops; `Channel::send` returns
  `ChannelError::Config` once the transport's mpsc receiver is gone.
- Message-send failures return errors to the upper layer without
  retrying (to avoid duplicates).

## Channel Implementations

Today the only in-tree transport is the gateway's `/v1/channel-ws`
server (`crates/gateway/src/channel/`). Every channel — the built-in
TUI and any out-of-process sidecar — reaches the agent through that
same endpoint:

| Channel  | ID prefix | Transport                                                                |
| -------- | --------- | ------------------------------------------------------------------------ |
| TUI      | `tui_`    | `/v1/channel-ws` (PSK-authenticated)                                     |
| Sidecars | `<name>_` | `/v1/channel-ws` (subprocess token, claims its own `channel_type`)       |

See [`tui.md`](./tui.md) for the TUI client side and
[`gateway.md`](./gateway.md) for the server side. The only public SDK
for third-party sidecars is the TypeScript package at
`sdks/channel-ts/`. Its primary surface is a `Channel` interface plus
`runChannel(channel)` entry point: the sidecar author implements
`onMessage`, `inbound(signal)`, and optionally `onApprovalRequested`
/ `onDelta` / `onNotice`, and the SDK handles the WebSocket + MessagePack
transport, `Register`/`Ack` handshake, UDS dial, frame dispatch,
concurrent approval spawning, and auto-reconnect with exponential
backoff + jitter on transient drops (disable with `reconnect: false`).
The SDK-provided default logger also forwards its own output as
`Frame::SidecarLog` frames while the WS is open — the gateway pushes
them into the same `LogBuffer` that backs `/v1/logs`, so sidecar lines
surface in the dashboard alongside gateway-internal tracing. Custom
loggers (pino / winston) stay local; attribution uses
`sidecar::<channel_type>[::<target>]`.
The UDS path + token are read from `AURA_CHANNEL_SOCKET` /
`AURA_CHANNEL_TOKEN` env vars — the contract a future sidecar
supervisor will set. `ChannelSpawner` (`crates/gateway/src/spawn.rs`)
is the primitive that injects these when it eventually gets wired
into a `PluginManager` / `Supervisor`; until then the sidecar's
launcher must export them (or call `runChannel` with explicit
`wsUrl` / `token`).
Raw wire types are re-exported under the `./wire` subpath for advanced
callers. There is no Rust SDK — the TUI
has its own private WS client, and the server is authoritative on the
wire format.

The first in-tree sidecar built on the SDK is the Telegram channel at
`channel-src/telegram/` (package `@aura/channel-telegram`). It uses
`grammy` for long-polling, maps Telegram `chat_id`s to stable UUIDv5
`session_id`s, and surfaces `Frame::ApprovalRequested` as an inline-
keyboard prompt in the originating chat. It's also the working example
of the full `Channel` contract — `inbound(signal)` pump, `onMessage` /
`onNotice` round-trip, concurrent `onApprovalRequested`, `onStop`
cleanup.

## Channel Registry

`ChannelRegistry` keeps two disjoint views of live channels plus a
shared `ApprovalGateMap` populated from each registered channel's
`approval_gate()`:

- **Sidecars** — one `Arc<Channel>` per `ChannelType`. A Telegram
  sidecar serves every Telegram user from a single process, so the 1:1
  `ChannelType → Channel` mapping is correct for that flavor.
- **Session-scoped clients** — many per `ChannelType`, keyed by
  `session_id`. Used by the built-in TUI so multiple TUI processes can
  each pin their own session without racing over the channel-type slot.

```rust
pub struct ChannelRegistry {
    // DashMap<ChannelType, Arc<Channel>>  — sidecars
    // DashMap<String, Arc<Channel>>       — session_clients keyed by session_id
    // Arc<ApprovalGateMap>                — gate fan-in (type- and session-level)
}

impl ChannelRegistry {
    pub fn new() -> Self;
    pub fn approval_gates(&self) -> Arc<ApprovalGateMap>;
    pub fn register(&self, channel: Arc<Channel>) -> Result<()>;
    pub fn unregister_sidecar(&self, channel_type: ChannelType) -> Result<()>;
    pub fn unregister_session(&self, session_id: &str) -> Result<()>;
    pub fn get_for(&self, channel_type: &ChannelType, session_id: &str) -> Option<Arc<Channel>>;
    pub fn get_sidecar(&self, channel_type: ChannelType) -> Option<Arc<Channel>>;
    pub fn list(&self) -> Vec<ChannelType>;                // sidecar channel types
    pub fn list_session_clients(&self) -> Vec<String>;     // session ids
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

`register` dispatches on `Channel::owned_session()`:

- `None` → sidecar slot. Fails with `ChannelError::DuplicateChannel`
  if another sidecar already owns the channel type.
- `Some(sid)` → session-scoped slot. Fails with
  `ChannelError::DuplicateSessionClient` if another client is already
  attached to that session id (regardless of channel type).

`get_for(channel_type, session_id)` hides the split: it prefers a
session-scoped match when the session has an attached client and falls
back to the type-level sidecar otherwise. The agent router uses this
to route `AgentOutput` so a TUI pinned to a session always receives
its own stream even if a type-level sidecar also happens to be
registered.

Bootstrap hands the same `Arc<ApprovalGateMap>` to `ToolExecutor` so
gates registered later are visible immediately without re-plumbing.
The gate map mirrors the registry's two-table split: per-channel-type
gates are consulted for sidecars and per-`(channel, session)` gates
win for session-scoped clients, so the TUI's approval modal on one
instance never resolves a different instance's pending tool call.

Design rules:

- Sidecars: 1:1 per `ChannelType`. Duplicate registration returns
  `ChannelError::DuplicateChannel`.
- Session-scoped clients: 1:1 per `session_id`. Duplicate registration
  returns `ChannelError::DuplicateSessionClient`.
- `unregister_sidecar` / `unregister_session` drop the handle and
  evict its approval gate; tool calls that arrive after disconnect
  fall back to the fail-closed `AutoDenyGate` for the appropriate
  scope.
- The `agent` Router owns `Arc<ChannelRegistry>` and calls `get_for()`
  for O(1) dispatch by `(ChannelType, session_id)`.

## Constraints

- `channels` stays independent of `agent`, `llm`, `tools`, and all other
  business crates (depends only on `model` and `session`; `aura-tools`
  is pulled in only for the `ApprovalGate` + `ApprovalGateMap` types)
- Transports own framing and encoding. This crate only defines the
  neutral `wire::{Frame, Message}` shapes (MessagePack-named) and the
  `encode` / `decode` helpers both sides call; the gateway route and
  the TUI's private WS client each own their own WebSocket plumbing.

## Collaboration

| Module     | Role                                                                           |
| ---------- | ------------------------------------------------------------------------------ |
| `model`    | Provides `ContentBlock`, `ChatMessage`, `ChannelType`, `ResourceAccess`        |
| `session`  | Provides `User`                                                                |
| `agent`    | Router owns the registry and dispatches `AgentOutput` by `ChannelType`         |
| `tools`    | Provides `ApprovalGate` + `ApprovalGateMap` reused by the registry             |
| `gateway`  | Hosts the only in-tree transport (`/v1/channel-ws`); builds and registers `Arc<Channel>` per connection |
| `security` | Input messages go to `SecurityGateway` first after entering the system         |
