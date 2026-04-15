# channels - Channel Ingress Layer

## Overview

The `channels` crate defines the **trait interface** for receiving messages from multiple platforms and converting them into a unified `IncomingMessage`, then converting `OutgoingMessage` back into platform-native formats for delivery.

**Design pattern**: Adapter pattern. The crate provides the `ChannelAdapter` trait, shared message types, and `ChannelRegistry`. Built-in adapters (e.g. `TuiAdapter` — a Ratatui-based terminal UI that is the default interactive channel) are implemented directly in this crate. Additional platform adapters (Telegram, Discord, HTTP, etc.) can be added as native crates behind the same trait.

Core responsibilities of this crate:

- Define the `ChannelAdapter` trait contract
- Define shared message types (`Message`, `IncomingMessage`, `OutgoingMessage`)
- Define error types (`ChannelError`)
- Provide `ChannelRegistry` for adapter registration, lookup, and lifecycle management

## Design Decisions

### Built-in and extensible adapters

This crate contains the `ChannelAdapter` trait and built-in adapters that require only pure-Rust dependencies (e.g. `TuiAdapter`, which pulls in `ratatui` + `crossterm`). Platform-specific adapters that bring SDK dependencies (Telegram, Discord, etc.) live in their own crates behind the same trait so their dependencies stay opt-in.

### No business logic

Channels contain no routing, rate limiting, or security logic. They depend only on `model` and `session`. Business logic belongs to `agent` and `security`.

### Optional streaming output

Adapters may opt into incremental rendering by overriding `send_stream_delta`.
The default implementation is a no-op, so channels that only support one-shot
delivery (HTTP responses, webhook posts) ignore deltas and wait for the final
`send_response` call. Adapters that can paint partial output — the built-in
`TuiAdapter`, for instance — accumulate deltas as they arrive and reconcile
against the canonical `OutgoingMessage` when the turn finishes. Delivery
ordering per `session_id` is the caller's responsibility; adapters assume
chunks arrive in the order the LLM emitted them.

### Out-of-band notices

`AgentOutput` carries three variants (`Delta`, `Message`, `Notice`).
`Notice { level: NoticeLevel, text }` is the path for events the user
didn't prompt for but should see — e.g. "a skill you invoked was rated
suspicious and was kept with a warning" or "…was blocked". The router
fans notices out to the per-session channel adapter via
`ChannelAdapter::send_notice`; the default trait method is a no-op so
transports without a side-channel (one-shot HTTP) can drop them. The
built-in TUI forwards notices into the same scrollback surface it uses
for `warn!` / `error!` tracing events, preserving colour coding.

### Unified message mapping

All platforms map to the same `IncomingMessage` structure via consistent ID prefixing (e.g. `tg_{msg_id}`, `dc_{msg_id}`) and session derivation rules.

### Error handling strategy

- Connection failures use exponential backoff for reconnection
- Message-send failures return errors to the upper layer without retrying (to avoid duplicates)

### Graceful shutdown

Router calls `stop()` on all channels, each exits its background loop and releases resources, with a global timeout for forced exit.

## Channel Implementations

Built-in adapters are implemented directly in this crate. Platform-specific adapters live in their own crates that implement the `ChannelAdapter` trait and are wired in by the bootstrap layer.

Current and planned adapters:

| Adapter  | ID prefix | Transport         |
| -------- | --------- | ----------------- |
| TUI (built-in) | `tui_`    | Terminal (Ratatui) |
| HTTP           | `http_`   | REST API (axum)   |
| Telegram       | `tg_`     | Long polling      |
| Discord        | `dc_`     | WebSocket Gateway |

The built-in `TuiAdapter` is the default interactive channel when `aura` is
launched with no subcommand. It renders a Ratatui chat scrollback plus an
input box, and opens dashboard views for bare dashboard-style slash commands
(`/skills`, `/tools`, `/jobs`, `/sessions`, `/memory`). See
[`tui.md`](./tui.md) for the full contract.

Each adapter must:

- Be `Send + Sync + 'static`
- Generate message IDs with its platform prefix
- Support graceful, idempotent shutdown
- Carry source, version, hash, trust level, and capability declarations per the governance model

## Channel Registry

`ChannelRegistry` manages the full lifecycle of channel adapters:

```rust
pub struct ChannelRegistry { /* HashMap<ChannelType, ChannelEntry> */ }

impl ChannelRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, adapter: Box<dyn ChannelAdapter>) -> Result<()>;
    pub async fn unregister(&mut self, channel_type: ChannelType) -> Result<()>;
    pub fn get(&self, channel_type: ChannelType) -> Option<&dyn ChannelAdapter>;
    pub async fn start_all(&mut self, sender: mpsc::Sender<IncomingMessage>) -> Result<()>;
    pub async fn stop_all(&mut self);
    pub fn list(&self) -> Vec<(ChannelType, &ChannelStatus)>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

Each registered adapter has a tracked `ChannelStatus`:

- **Registered** — adapter is registered but not yet started
- **Running** — adapter is actively listening for messages
- **Stopped** — adapter has been gracefully stopped
- **Error(reason)** — adapter encountered an error during start or runtime

Design rules:

- One adapter per `ChannelType` — duplicate registration returns `ChannelError::DuplicateChannel`
- `unregister` stops a running adapter before removal
- `start_all` skips already-running adapters
- `stop_all` is best-effort — continues on individual failures
- The `agent` Router owns the `ChannelRegistry` and uses `get()` for O(1) dispatch by `ChannelType`

## Constraints

- `channels` (the crate) stays independent of `agent`, `llm`, `tools`, and all other business crates (depends only on `model` and `session`)
- Each adapter must be `Send + Sync + 'static` for safe use across tokio tasks
- Platform SDK dependencies belong in their own adapter crates, not in this crate; built-in adapters must have no external dependencies

## Collaboration

| Module     | Role                                                                        |
| ---------- | --------------------------------------------------------------------------- |
| `model`    | Provides `ContentBlock`, `ChatMessage`, and other content primitives        |
| `session`  | Provides `ChannelType`, `User`                                              |
| `agent`    | Router registers adapters and dispatches outgoing messages by `ChannelType` |
| `security` | Input messages go to `SecurityGateway` first after entering the system      |
