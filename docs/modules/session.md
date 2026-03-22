# session - Session Management

## 1. Module Overview

**Responsibility**: session lifecycle management, including session creation, retrieval, update, expiration cleanup, and the definition of the `SessionStore` trait.

**Design principle**: the `session` crate is separated from the `storage` crate and follows the rule that traits are defined in their own crate. The `SessionStore` trait is defined in `session`, while the concrete implementation is provided by `storage` (such as `SqliteSessionStore`). This guarantees a single dependency direction: `storage` depends on `session`, not the reverse.

The `session` crate contains no storage implementation logic. It only defines abstract interfaces and the manager.

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Imported Types |
|------------|---------|
| `core` | `Session`, `User`, `ChannelType`, `SessionState`, `AuraError` |

`session` depends only on the foundational data types in `core` and on no other business crate.

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `async-trait` | Async methods for `SessionStore` |
| `chrono` | Time handling (`DateTime<Utc>` for expiration checks) |
| `serde` | Session serialization and deserialization (inherited from `core::Session`) |

### 2.3 Dependency Direction

```text
core (Session, User, ChannelType, SessionState)
  ^
  |
session (SessionStore trait, SessionManager)
  ^
  |
storage (concrete implementations such as SqliteSessionStore)
  ^
  |
agent (uses SessionManager via Router)
```

## 3. Public Interfaces

### 3.1 SessionStore Trait

`SessionStore` is the abstract interface for session persistence. Concrete implementations are provided by the `storage` crate.

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Get a session by session ID.
    /// Returns None if the session does not exist
    /// (either expired or never created).
    async fn get(&self, session_id: &str) -> Result<Option<Session>>;

    /// Save or update a session.
    /// If the session ID already exists, update it; otherwise insert a new record.
    async fn save(&self, session: &Session) -> Result<()>;

    /// Delete a session by ID.
    /// Used for expiration cleanup or when the user explicitly ends the session.
    async fn delete(&self, session_id: &str) -> Result<()>;

    /// List session IDs whose last activity was before the given time.
    /// Used for batch expiration cleanup.
    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>>;
}
```

Concurrency-safety conventions:

- The trait requires `Send + Sync`, so implementations can be shared safely across threads
- Concrete implementations such as SQLite must ensure correctness of concurrent writes themselves, usually via connection pools or write locks
- In the Aura architecture, concurrent access to the same session is serialized by the Actor model at the upper layer, so `SessionStore` does not need to solve write conflicts for the same `session_id`

### 3.2 SessionManager

`SessionManager` wraps `SessionStore` and provides higher-level session-management logic.

```rust
pub struct SessionManager {
    store: Box<dyn SessionStore>,
    session_timeout: Duration,
}

impl SessionManager {
    pub fn new(store: Box<dyn SessionStore>, session_timeout: Duration) -> Self;
    pub async fn create_session(&self, user: User, channel: ChannelType) -> Result<Session>;
    pub async fn get_or_create(
        &self,
        session_id: &str,
        user: User,
        channel: ChannelType,
    ) -> Result<Session>;
    pub async fn touch(&self, session_id: &str) -> Result<()>;
    pub async fn cleanup_expired(&self) -> Result<usize>;
    pub fn session_timeout(&self) -> Duration;
}
```

## 4. Implementation Details

### 4.1 Session ID Generation Strategy

It is recommended to use **UUID v4** as the session ID generation strategy:

- UUID v4 is a purely random 128-bit identifier with extremely low collision probability
- Session IDs do not need time ordering, unlike Trace or Job, so ULID's ordering benefit does not apply here
- The `uuid` crate is mature and stable in the Rust ecosystem

Example:

```rust
let session_id = uuid::Uuid::new_v4().to_string();
```

### 4.2 Relationship Between Session and User

`Session` is associated through `user: User` and `channel: ChannelType`:

```rust
struct Session {
    id: String,
    user: User,
    channel: ChannelType,
    messages: Vec<ChatMessage>,
    created_at: DateTime<Utc>,
    last_active: DateTime<Utc>,
    state: SessionState,
}
```

**Session-isolation policy**: the same `User` has independent sessions on different `Channel`s such as Slack, Discord, or Web. That means:

- Alice's conversation history on Slack will not appear in her Discord session
- Each `(user_id, channel)` pair maps to an independent session instance
- `get_or_create` must consider both user and channel when locating a session

### 4.3 Session Timeout Flow

```text
1. CronScheduler periodically triggers a cleanup task
         |
2. Call SessionManager::cleanup_expired()
         |
3. Compute cutoff time: now - session_timeout
         |
4. Call store.list_expired(cutoff) to get expired session IDs
         |
5. For each expired session:
   a. Send SessionTimeout to the corresponding AgentActor through AgentSupervisor
   b. AgentActor performs cleanup (release resources, trigger SessionDestroyed hook)
   c. Call store.delete(session_id) to remove persisted data
```

### 4.4 Handling Concurrent Access to the Same Session

Aura uses the **Actor model** to guarantee serialized handling within the same session:

- Each active session maps to one `AgentActor`
- `AgentSupervisor` maintains `HashMap<SessionId, ActorHandle<AgentMessage>>`
- Every message targeting the same session, whether user input, cron triggers, rollback requests, or timeout notifications, is queued through the actor handle
- `AgentActor` consumes that queue sequentially, so no concurrent mutation occurs inside the same session

Therefore, `SessionStore` implementations do not need to handle write conflicts on the same `session_id`. That guarantee is provided by the upper-layer Actor model.

### 4.5 SessionState Use Cases

```rust
pub struct SessionState {
    pub active_skill: Option<String>,
    pub compression_count: u32,
    pub extra: HashMap<String, Value>,
}
```

Use cases:

| Field | Scenario |
|------|------|
| `active_skill` | Set when the user enters a multi-turn interaction flow of a skill, then cleared back to `None` after completion. `AgentLoop` uses it to decide whether to route messages to a skill handler. |
| `compression_count` | Incremented after each context compression by `ContextManager`. It can be used for monitoring or for switching to more aggressive compression strategies. |
| `extra` | Reserved extension fields stored as arbitrary key-value pairs, useful for experimental features or plugin-defined state. |

## 5. File Structure

```text
crates/session/src/
├── lib.rs
└── manager.rs
```

- `lib.rs`: defines the `SessionStore` trait and re-exports public types
- `manager.rs`: implements `SessionManager`, including create/get/cleanup logic

## 6. Configuration

Session-related configuration:

| Config | Type | Default | Description |
|--------|------|--------|------|
| `session.timeout` | `Duration` | `30m` | Session timeout. Sessions inactive longer than this will be cleaned up. |
| `session.cleanup_interval` | `Duration` | `5m` | Cleanup interval for expired sessions, driven by CronScheduler. |

Configuration example:

```json
{
  "session": {
    "timeout_minutes": 30,
    "cleanup_interval_minutes": 5
  }
}
```

## 7. Interaction with Other Modules

### 7.1 Router -> SessionManager

`Router` is the main caller of the session module. When a message arrives from a channel:

```text
IncomingMessage -> Router
                     |
                     ├── session_manager.get_or_create(session_id, user, channel)
                     |       -> get or create Session
                     |
                     ├── session_manager.touch(session_id)
                     |       -> update last-active time
                     |
                     └── agent_supervisor.route(session_id, message)
                             -> deliver the message to the corresponding AgentActor
```

### 7.2 AgentActor Holds a Session Instance

```rust
pub struct AgentActor {
    session: Session,
    agent_loop: AgentLoop,
    response_tx: mpsc::Sender<OutgoingMessage>,
    hooks: HookManager,
}
```

- `AgentActor` receives a `Session` when created and keeps its in-memory state
- `AgentLoop::run` receives `&mut Session` and updates `messages`, `state`, and so on during the conversation loop
- The timing of persistence is coordinated between `SessionManager` and `AgentActor`, such as saving after each completed turn

### 7.3 storage Provides the SessionStore Implementation

```rust
pub struct SqliteSessionStore { pool: SqlitePool }
impl SessionStore for SqliteSessionStore { ... }
```

- `StorageFactory` creates `SqliteSessionStore` or the in-memory implementation based on configuration and packages it into `StorageSet`
- The `agent` assembly layer injects `StorageSet.session` into `SessionManager`
- This achieves full dependency inversion: `SessionManager` depends only on the `SessionStore` trait, not on a concrete backend

### 7.4 SessionCreated / SessionDestroyed Hooks

The `hook` crate defines `HookPoint::SessionCreated` and `HookPoint::SessionDestroyed`:

- `SessionCreated`: triggered after `SessionManager::create_session` succeeds, useful for welcome messages or audit logs
- `SessionDestroyed`: triggered when a session expires or is explicitly deleted, useful for resource cleanup or reporting

### 7.5 Interaction Overview

```text
  channels         agent              session            storage
     |               |                   |                  |
     |--message-->  Router               |                  |
     |            get_or_create -------> SessionManager     |
     |               |                   |---get----------> SessionStore
     |               |                   |<--Option<Session>-|
     |               |                   |---save---------> SessionStore
     |               |                   |                  |
     |            route to           AgentActor             |
     |            AgentActor         (holds Session)        |
     |               |                   |                  |
     |            cleanup_expired -----> SessionManager     |
     |               |                   |---list_expired-> SessionStore
     |               |                   |---delete-------> SessionStore
```
