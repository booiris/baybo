# channels - Channel Ingress Layer

## 1. Module Overview

**Responsibility**: provide a unified way to receive messages from multiple channels and convert raw platform messages from Telegram, Discord, HTTP API, and CLI into `core::IncomingMessage`, then convert `core::OutgoingMessage` back into platform-native formats for delivery.

**Design pattern**: Adapter pattern. Each channel implements the same `ChannelAdapter` trait, hiding platform-specific details so upper layers such as Router and AgentActor do not need to understand channel differences.

**Core responsibility boundary**:

- Receive, transform, and send messages
- Manage SDK connections and platform lifecycle
- Contain no business logic such as routing, rate limiting, or security checks
- Depend only on `core`, not on other business crates

**Design decisions**:

- No streaming output support; every channel sends only once the full response is ready
- v1 supports multimodal input and output such as image, audio, and file

---

## 2. Dependencies

### 2.1 Internal Dependencies

Only depends on `core`, using types such as:

- `Message`
- `IncomingMessage`
- `OutgoingMessage`
- `ChannelType`
- `ContentBlock`
- `User`
- `ChannelMetadata`
- `AuraError`

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `tokio` | Async runtime, `mpsc`, signal handling |
| `async-trait` | Async traits |
| `teloxide` | Telegram Bot API SDK |
| `serenity` | Discord Bot API SDK |
| `axum` | HTTP service framework for `HttpChannel` |
| `tower-http` | HTTP-layer middleware such as CORS |
| `serde` / `serde_json` | Config and message serialization |
| `tracing` | Structured logs |
| `bytes` | Multimodal binary handling |

### 2.3 Dependency Direction

```text
core
  ^
  |
channels
  ^
  |
agent
```

`channels` stays independent of `agent`, `llm`, `tools`, `session`, and all other business crates.

---

## 3. Public Interfaces

### 3.1 ChannelAdapter Trait

```rust
#[async_trait]
pub trait ChannelAdapter: Send + Sync + 'static {
    fn channel_type(&self) -> ChannelType;
    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()>;
    async fn send_response(&self, response: OutgoingMessage) -> Result<()>;
    async fn stop(&self) -> Result<()>;
}
```

Semantics:

- `channel_type()`: pure, stable channel identifier
- `start()`: start listening in the background and push `IncomingMessage` into the system
- `send_response()`: convert an `OutgoingMessage` into the native platform format and send it
- `stop()`: graceful shutdown, idempotent, and resource-cleaning

Trait-object constraints:

- `Send`: safe to move across tokio tasks
- `Sync`: safe to call concurrently where needed
- `'static`: no borrowed lifetime, safe for long-lived containers

### 3.2 Concrete Implementations

#### TelegramChannel

- Based on `teloxide`
- Uses long polling by default, with webhook possible later
- Supports text, image, audio, and file conversion
- Uses `send_message`, `send_photo`, `send_audio`, and `send_document`

#### DiscordChannel

- Based on `serenity`
- Receives messages through Gateway WebSocket events
- Can support slash commands through interaction handlers
- Sends replies and attachments through the Discord message API

#### HttpChannel

- Based on `axum`
- Exposes a REST API
- Uses request-response semantics rather than push delivery
- Can return multimodal data through JSON plus Base64 encoding

#### CliChannel

- Local development and debugging only
- Implements a REPL through stdin/stdout
- Supports `/quit` and optional debug commands

---

## 4. Implementation Details

### 4.1 Unified Message Mapping

The common mapping into `IncomingMessage`:

| Field | Telegram | Discord | HTTP | CLI |
|------|----------|---------|------|-----|
| `id` | `"tg_{msg.message_id}"` | `"dc_{msg.id}"` | `"http_{uuid}"` | `"cli_{uuid}"` |
| `session_id` | `"tg_{chat.id}"` | `"dc_{channel_id}"` | Provided by request or auto-generated | `"cli_default"` |
| `sender.id` | `"tg_{from.id}"` | `"dc_{author.id}"` | `"http_{user_id}"` | `"cli_user"` |
| `sender.name` | `from.first_name` | `author.name` | From request if present | `"CLI User"` |
| `channel` | `ChannelType::Telegram` | `ChannelType::Discord` | `ChannelType::Http` | `ChannelType::Cli` |
| `content` | Multimodal mapping | Multimodal mapping | JSON parsing | `ContentBlock::Text` |

### 4.2 Multimodal Handling Strategy

Inbound:

| Content Type | Telegram | Discord | HTTP |
|----------|----------|---------|------|
| Text | `msg.text` | `msg.content` | `request.content` |
| Image | Download from `msg.photo` | Filter image attachments | Base64 decode |
| Audio | Download from `voice` / `audio` | Filter audio attachments | Base64 decode |
| File | Download from `document` | Other attachments | Base64 decode |

Outbound:

| Content Type | Telegram | Discord | HTTP | CLI |
|----------|----------|---------|------|-----|
| Text | `send_message` | send text message | JSON text block | `println!` |
| Image | `send_photo` | attachment | Base64 JSON | placeholder text |
| Audio | `send_audio` | attachment | Base64 JSON | placeholder text |
| File | `send_document` | attachment | Base64 JSON | placeholder text |

### 4.3 Error Handling

Connection reconnection:

- Use exponential backoff for long-polling or WebSocket reconnect loops
- Reset backoff after successful receive
- Log and retry on transient platform failures

Message-send failures:

- Return `Err(AuraError)` to the upper layer
- The channel layer does not retry sends on its own, to avoid duplicates
- For HTTP 429, record and surface `Retry-After` metadata for operators

### 4.4 Graceful Shutdown

Typical flow:

1. The system receives SIGTERM or SIGINT
2. Router calls `stop()` on all channels
3. Each channel exits its background loop and releases resources
4. Router waits for all shutdowns with a global timeout
5. If the timeout is reached, the process exits forcefully

### 4.5 Channel Registration in Router

Channels are registered by the `agent` assembly layer via dependency injection:

- Build a list of enabled `ChannelAdapter`s from config
- Start each adapter with a shared `mpsc::Sender<IncomingMessage>`
- Pass the adapter set into Router
- Router dispatches `OutgoingMessage` according to `ChannelType`

---

## 5. File Structure

```text
crates/channels/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── telegram.rs
    ├── discord.rs
    ├── http.rs
    └── cli.rs
```

Feature gating is recommended so Telegram and Discord dependencies are only compiled when needed.

---

## 6. Configuration

Example `channels` section:

```json
{
  "channels": {
    "http": {
      "enabled": true,
      "bind_addr": "0.0.0.0:3000",
      "cors_origins": ["*"]
    },
    "telegram": {
      "enabled": false,
      "bot_token": "${TELEGRAM_BOT_TOKEN}"
    },
    "discord": {
      "enabled": false,
      "bot_token": "${DISCORD_BOT_TOKEN}"
    },
    "cli": {
      "enabled": true
    }
  }
}
```

Sensitive tokens should come from environment variables rather than be hardcoded into config.

---

## 7. Extension Guide

To add a new channel implementation:

1. Create a new adapter type such as `SlackChannel`
2. Implement all four methods of `ChannelAdapter`
3. Add a new `ChannelType` variant in `core`
4. Put the SDK dependency behind a feature gate
5. Register the adapter from the `agent` bootstrap layer based on config
6. Add config examples and docs
