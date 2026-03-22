# core - Shared Foundational Types

## 1. Module Overview

The `core` crate is Aura's lowest-level shared data model. It only provides foundational types exchanged across modules and contains no business traits.

Core contents include:

- Message models: `Message`, `ContentBlock`, `ChatMessage`
- Session models: `Session`, `SessionState`
- User models: `User`, `ChannelType`
- Metadata models: `MessageMetadata`
- Error type: `AuraError`
- Operation type: `OperationKind`

Design constraints:

- `core` does not depend on any other crate in the workspace
- `core` does not define business interfaces such as `ChannelAdapter`, `SessionStore`, or `ContextManager`
- All upper layers use `core` only as a data exchange layer

---

## 2. Dependencies

### 2.1 External Dependencies

| Dependency | Purpose |
|------|------|
| `serde` / `serde_json` | Type serialization |
| `chrono` | Timestamps |
| `anyhow` | Generic error wrapping |

### 2.2 Dependent Modules

`core` is depended on by all other business modules, including:

- `channels`
- `llm`
- `tools`
- `skills`
- `memory`
- `context`
- `session`
- `trace`
- `job`
- `security`
- `cost`
- `hook`
- `storage`
- `agent`

---

## 3. Public Interfaces

### 3.1 Message

```rust
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub channel: ChannelType,
    pub sender: User,
    pub content: Vec<ContentBlock>,
    pub timestamp: DateTime<Utc>,
    pub reply_to: Option<String>,
    pub metadata: MessageMetadata,
}
```

`Message` represents one complete raw message received from a channel and is intended for the ingress and security layers.

### 3.2 ContentBlock / BlobRef

```rust
pub enum ContentBlock {
    Text(String),
    Image { blob: BlobRef, mime_type: String },
    Audio { blob: BlobRef, mime_type: String },
    File  { blob: BlobRef, filename: String, mime_type: String },
}

pub struct BlobRef {
    pub blob_id: String,
    pub size_bytes: u64,
    pub sha256: String,
}
```

The key design here is: **multimedia content is passed only by reference, and raw binary data is never embedded directly into `Message`, `Session`, `Trace`, or `Snapshot`.**

Why:

- Prevent sessions and snapshots from growing without bound
- Prevent Trace from copying large objects
- Allow media data to be stored separately in object storage or blob tables

### 3.3 ChatMessage

```rust
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}
```

`ChatMessage` is the lightweight message structure used for LLM requests. It keeps only role and content, without channel metadata.

### 3.4 MessageMetadata

```rust
pub struct MessageMetadata {
    pub channel_specific: Option<ChannelMetadata>,
    pub priority: Option<MessagePriority>,
    pub thread_id: Option<String>,
    pub extra: HashMap<String, Value>,
}
```

### 3.5 Session / SessionState

```rust
pub struct Session {
    pub id: String,
    pub user: User,
    pub channel: ChannelType,
    pub messages: Vec<ChatMessage>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub state: SessionState,
}

pub struct SessionState {
    pub active_skill: Option<String>,
    pub compression_count: u32,
    pub extra: HashMap<String, Value>,
}
```

`Session.messages` stores logical message history and must not carry raw media bytes.

### 3.6 User / ChannelType

```rust
pub struct User {
    pub id: String,
    pub name: Option<String>,
    pub channel: ChannelType,
}

pub enum ChannelType {
    Telegram,
    Discord,
    Http,
    Cli,
}
```

### 3.7 AuraError

```rust
pub enum AuraError {
    Internal(anyhow::Error),
    Config(String),
    Serialization(String),
    Io(std::io::Error),
}
```

### 3.8 OperationKind

```rust
pub enum OperationKind {
    LlmCall { model: String },
    ToolExecution { tool_name: String },
    SkillExecution { skill_name: String },
    CronExecution { cron_job_id: String },
    ContextCompression { strategy: String },
    MemoryOperation { operation: String },
    UserMessageHandling { session_id: String },
}
```

`OperationKind` lives in `core` because it is shared by both `job` and `trace`, making it a cross-module identifier type.

---

## 4. Implementation Details

### 4.1 Why `core` Does Not Define Traits

If business traits were placed in `core`, two problems would appear:

1. `core` would quickly inflate into a global dumping ground
2. Module boundaries would be broken and dependency direction would easily invert

So the rule is:

- Traits are defined in their own modules
- Shared data types are defined in `core`

### 4.2 The Value of Typed Metadata

Both `MessageMetadata` and `SessionState` use a "typed fields + `extra` escape hatch" design instead of pure `HashMap<String, Value>`:

- Common fields can be checked at compile time
- Dynamic extension space still exists
- Flexibility does not require giving up all type safety

### 4.3 Media References Instead of Inline Binary Data

This is one of the most important foundational changes in the new architecture. If raw image, audio, or file bytes were placed directly in `ContentBlock`:

- `Session` would become large
- `ContextSnapshot` would duplicate that data
- `Trace` would grow even further

After switching to `BlobRef`:

- The conversation context keeps only references
- Actual media storage is handled by upper layers or the storage layer
- Rollback and replay become much more stable in cost

### 4.4 Clone and Thread Safety

`core` types move frequently through the Actor model, async message passing, and Trace snapshots, so they should satisfy:

- `Send + Sync`
- Serializable
- `Clone` for most types

`AuraError` does not need `Clone`, because wrapped lower-level errors are usually not safe to copy.

### 4.5 Serialization Constraints

It is recommended that all foundational types derive `Serialize` / `Deserialize`, with the following caveats:

- `AuraError` is not suitable for direct persistence
- `BlobRef` is safe to store
- The `extra` field must contain only serializable JSON values

---

## 5. File Structure

```text
crates/core/src/
├── lib.rs
├── message.rs
├── session.rs
├── user.rs
├── operation.rs
└── error.rs
```

Suggested responsibility split:

- `message.rs`: messages, content blocks, roles, inbound/outbound messages
- `session.rs`: sessions and session state
- `user.rs`: users and channel types
- `operation.rs`: operation enum shared across modules
- `error.rs`: foundational error type

---

## 6. Implementation Recommendations

- Do not manage the lifecycle of the actual media referenced by `BlobRef` inside `core`
- `OutgoingMessage` / `IncomingMessage` can remain in `message.rs` as channel-adaptation wrappers around `Message`
- Any field that may enter logs, Trace, or Job should be designed as sanitizable and serializable by default
