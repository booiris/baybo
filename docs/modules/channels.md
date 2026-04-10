# channels - Channel Ingress Layer

## Overview

The `channels` crate defines the **trait interface** for receiving messages from multiple platforms and converting them into a unified `IncomingMessage`, then converting `OutgoingMessage` back into platform-native formats for delivery.

**Design pattern**: Adapter pattern. The crate provides the `ChannelAdapter` trait and shared message types. Concrete implementations (Telegram, Discord, HTTP, CLI, etc.) live **outside** this crate in the top-level `channels/` directory as independent WASM modules, loaded at runtime via the WASM extension mechanism in `sandbox`.

Core responsibilities of this crate:

- Define the `ChannelAdapter` trait contract
- Define shared message types (`Message`, `IncomingMessage`, `OutgoingMessage`)
- Define error types (`ChannelError`)
- Provide `ChannelRegistry` for adapter registration, lookup, and lifecycle management

## Design Decisions

### Trait-only crate

This crate contains **no concrete implementations**. All platform adapters are built as WASM modules under the top-level `channels/` directory, keeping platform SDK dependencies out of the main workspace and enabling hot-reload and sandboxed execution.

### No business logic

Channels contain no routing, rate limiting, or security logic. They depend only on `model` and `session`. Business logic belongs to `agent` and `security`.

### No streaming output

Every channel sends only once the full response is ready. This simplifies the adapter interface and avoids partial-delivery complexity.

### Unified message mapping

All platforms map to the same `IncomingMessage` structure via consistent ID prefixing (e.g. `tg_{msg_id}`, `dc_{msg_id}`) and session derivation rules.

### Error handling strategy

- Connection failures use exponential backoff for reconnection
- Message-send failures return errors to the upper layer without retrying (to avoid duplicates)

### Graceful shutdown

Router calls `stop()` on all channels, each exits its background loop and releases resources, with a global timeout for forced exit.

## WASM Channel Implementations

Concrete channel adapters are built as WASM modules under `channels/` at the project root. Each module implements the `ChannelAdapter` trait and is loaded at runtime through the `sandbox` crate's WASM runtime.

Expected adapters:

| Adapter  | ID prefix | Transport         |
| -------- | --------- | ----------------- |
| CLI      | `cli_`    | stdin/stdout      |
| HTTP     | `http_`   | REST API (axum)   |
| Telegram | `tg_`     | Long polling      |
| Discord  | `dc_`     | WebSocket Gateway |

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

- `channels` (the crate) stays independent of `agent`, `llm`, `tools`, `session`, and all other business crates
- Each adapter must be `Send + Sync + 'static` for safe use across tokio tasks
- Platform SDK dependencies belong in the WASM modules, not in this crate

## Collaboration

| Module     | Role                                                                        |
| ---------- | --------------------------------------------------------------------------- |
| `model`    | Provides `ContentBlock`, `ChatMessage`, and other content primitives        |
| `session`  | Provides `ChannelType`, `User`                                              |
| `agent`    | Router registers adapters and dispatches outgoing messages by `ChannelType` |
| `security` | Input messages go to `SecurityGateway` first after entering the system      |
| `sandbox`  | Provides WASM runtime for loading channel adapter modules                   |
