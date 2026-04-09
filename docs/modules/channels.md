# channels - Channel Ingress Layer

## Overview

The `channels` crate provides a unified way to receive messages from multiple platforms (Telegram, Discord, HTTP API, CLI) and convert them into `core::IncomingMessage`, then convert `core::OutgoingMessage` back into platform-native formats for delivery.

**Design pattern**: Adapter pattern. Each channel implements the `ChannelAdapter` trait, hiding platform details so upper layers don't need to understand channel differences.

Core responsibilities:

- Receive, transform, and send messages
- Manage SDK connections and platform lifecycle
- Support multimodal input/output (text, image, audio, file)

## Design Decisions

### No business logic

Channels contain no routing, rate limiting, or security logic. They depend only on `core`. Business logic belongs to `agent` and `security`.

### No streaming output

Every channel sends only once the full response is ready. This simplifies the adapter interface and avoids partial-delivery complexity.

### Unified message mapping

All platforms map to the same `IncomingMessage` structure via consistent ID prefixing (e.g. `tg_{msg_id}`, `dc_{msg_id}`) and session derivation rules.

### Error handling strategy

- Connection failures use exponential backoff for reconnection
- Message-send failures return errors to the upper layer without retrying (to avoid duplicates)

### Graceful shutdown

Router calls `stop()` on all channels, each exits its background loop and releases resources, with a global timeout for forced exit.

## Constraints

- `channels` stays independent of `agent`, `llm`, `tools`, `session`, and all other business crates
- Each adapter must be `Send + Sync + 'static` for safe use across tokio tasks
- Platform SDK dependencies should be behind feature gates

## Collaboration

| Module | Role |
|--------|------|
| `core` | Provides `Message`, `IncomingMessage`, `OutgoingMessage`, `ChannelType` |
| `agent` | Router registers adapters and dispatches outgoing messages by `ChannelType` |
| `security` | Input messages go to `SecurityGateway` first after entering the system |
