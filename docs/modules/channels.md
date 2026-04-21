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
  speak the same protocol

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
`sdks/channel-ts/`, which consumes the same `wire` types via ts-rs
bindings. There is no Rust SDK — the TUI has its own private WS
client, and the server is authoritative on the wire format.

## Channel Registry

`ChannelRegistry` holds one `Arc<Channel>` per `ChannelType` behind a
`DashMap` and keeps a shared `ApprovalGateMap` populated from each
registered channel's `approval_gate()`:

```rust
pub struct ChannelRegistry { /* DashMap<ChannelType, Arc<Channel>> + Arc<ApprovalGateMap> */ }

impl ChannelRegistry {
    pub fn new() -> Self;
    pub fn approval_gates(&self) -> Arc<ApprovalGateMap>;
    pub fn register(&self, channel: Arc<Channel>) -> Result<()>;
    pub fn unregister(&self, channel_type: ChannelType) -> Result<()>;
    pub fn get(&self, channel_type: ChannelType) -> Option<Arc<Channel>>;
    pub fn list(&self) -> Vec<ChannelType>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

`register` and `unregister` are sync (no `async`): they only touch the
registry's own maps. Bootstrap hands the same `Arc<ApprovalGateMap>`
to `ToolExecutor` so gates registered later are visible immediately
without re-plumbing.

Design rules:

- One channel per `ChannelType` — duplicate registration returns
  `ChannelError::DuplicateChannel`.
- `unregister` drops the channel handle and evicts its approval gate;
  tool calls that arrive after disconnect fall back to the
  fail-closed `AutoDenyGate` for that channel type.
- The `agent` Router owns `Arc<ChannelRegistry>` and uses `get()` for
  O(1) dispatch by `ChannelType`.

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
