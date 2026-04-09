# memory - Long-Term Memory System

## 1. Module Overview

The `memory` module manages the full lifecycle of long-term user memory, including storage, retrieval, semantic search, and expiration cleanup.

**Core responsibilities:**

- Store important memories produced during user interaction, such as preferences, facts, and summaries
- Recall relevant historical memories for the current conversation and inject them as context enhancement
- Support vector-embedding semantic search when an embedder is configured, and fall back to keyword matching when it is not
- Decide automatically whether a new memory should be stored, avoiding redundancy
- Enforce expiration cleanup and per-user memory-count limits

**Position in the system**: `memory` is a middle-layer crate used directly by `agent::AgentLoop`. Each turn, Agent Loop calls `MemoryManager.recall()` before building context and calls `maybe_store()` after producing the final reply. In long-running mode, heartbeat, routine, and periodic cleanup tasks may also use memory indirectly through `agent`, but they still must not bypass Job and Trace.

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Types Used |
|------------|-----------|
| `core` | `User`, `Session`, `ContentBlock`, `AuraError`, `OperationKind` |

### 2.2 External Dependencies

| Crate | Purpose |
|-------|------|
| `serde` / `serde_json` | Serialization of `MemoryEntry` and `MemoryCategory` |
| `chrono` | `DateTime<Utc>` timestamp fields |
| `async-trait` | Async trait support for `MemoryStore` |
| `rig` | `rig::embeddings::EmbeddingModel` for vector embedding generation |

### 2.3 Explicit Non-Dependencies

To keep dependency direction clean and acyclic, `memory` **does not depend on**:

- `llm`: `memory` does not call LLMs itself. Any logic that needs LLM assistance, such as importance scoring, is performed by upper layers and then passed in
- `agent`: `memory` is used by `agent`, never the reverse
- `storage`: `MemoryStore` is defined in `memory`, while implementations such as `SqliteMemoryStore` live in `storage`
- `workspace`: identity files and heartbeat policy belong to `workspace`; `memory` manages retrievable memory only

```text
core  ──►  memory  ◄──  agent
                    ◄──  storage
```

---

## 3. Public Interfaces

### 3.1 MemoryStore Trait

```rust
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store(&self, entry: &MemoryEntry) -> Result<()>;
    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>>;
    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>>;
    async fn delete(&self, id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>>;
}
```

Return-value conventions:

- All methods return `Result<T>` with `core::AuraError`
- `retrieve()` returns `Option<MemoryEntry>` rather than treating "not found" as an error
- `search()` may return an empty vector
- `delete()` should be idempotent for missing IDs

Known implementations:

| Implementation | Location | Purpose |
|------|-----------|------|
| `SqliteMemoryStore` | `storage/src/sqlite/memory.rs` | Production, based on SQLite |
| In-memory backend | `storage/src/memory_backend/` | Development and testing |

### 3.2 MemoryEntry

```rust
pub struct MemoryEntry {
    pub id: String,
    pub user_id: String,
    pub content: String,
    pub category: MemoryCategory,
    pub importance: f32,
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub source_session_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

Field notes:

| Field | Type | Required | Description |
|------|------|------|------|
| `id` | `String` | Yes | UUID generated on creation |
| `user_id` | `String` | Yes | Associated `core::User.id` |
| `content` | `String` | Yes | Plain-text memory content |
| `category` | `MemoryCategory` | Yes | Semantic category |
| `importance` | `f32` | Yes | Importance in the range 0.0 to 1.0 |
| `embedding` | `Option<Vec<f32>>` | No | Vector embedding if configured |
| `created_at` | `DateTime<Utc>` | Yes | Creation time |
| `last_accessed` | `DateTime<Utc>` | Yes | Last recall time |
| `source_session_id` | `Option<String>` | No | Source session |
| `expires_at` | `Option<DateTime<Utc>>` | No | Expiration time; `None` means never expires |

### 3.3 MemoryCategory

```rust
pub enum MemoryCategory {
    UserPreference,
    KeyFact,
    InteractionSummary,
    Custom(String),
}
```

Typical usage:

| Category | Meaning | Example |
|------|------|---------|
| `UserPreference` | Explicit or implicit user preferences | "The user prefers concise answers" |
| `KeyFact` | Important facts related to the user | "The user's project uses Rust + PostgreSQL" |
| `InteractionSummary` | A compressed summary of a past interaction | "In the 2026-03-20 session, the user chose incremental migration" |
| `Custom(String)` | Custom domain-specific categories | `Custom("project_context")` |

### 3.4 MemoryManager

```rust
pub struct MemoryManager {
    store: Box<dyn MemoryStore>,
    embedder: Option<Box<dyn rig::embeddings::EmbeddingModel>>,
}
```

Construction:

```rust
impl MemoryManager {
    pub fn without_embedder(store: Box<dyn MemoryStore>) -> Self;
}
```

The optional embedder integration is currently an internal implementation detail. The public API keeps `without_embedder()` as the stable constructor used by the workspace today.

Public methods:

```rust
impl MemoryManager {
    pub async fn recall(&self, content: &[ContentBlock]) -> Result<Vec<MemoryEntry>>;
    pub async fn maybe_store(&self, session: &Session, response: &str) -> Result<()>;
    pub async fn store(&self, entry: MemoryEntry) -> Result<()>;
    pub async fn forget_expired(&self) -> Result<usize>;
}
```

---

## 4. Implementation Details

### 4.1 Recall Strategy

`recall()` is a key step in every Agent Loop turn.

With an embedder:

1. Extract and join the text parts from `content`
2. Generate a query vector with `embedder`
3. Compare against candidate memory embeddings using cosine similarity
4. Rank by similarity, then blend in `importance`
5. Return the top-N results

Without an embedder:

1. Extract keywords from `content`
2. Call `store.search(user_id, query, limit)`
3. Sort by descending `importance`

Common post-processing:

- Limit recall count to avoid context overflow
- Prioritize more important memories
- Update `last_accessed` for recalled items

### 4.2 Automatic Memory Storage

`maybe_store()` runs after the final response is produced and decides whether the current turn should become memory.

Typical triggers:

- Preference expressions such as "I like..." or "Please use this style..."
- Important factual information about the user or project
- Interaction length crossing a summary threshold
- Heuristic rule-engine matches

Importance scoring:

- `memory` itself does **not** call LLMs
- Rule-based defaults can be used, for example higher defaults for preferences and key facts
- Upper layers may still adjust `importance` before calling `store()`

Deduplication:

- Check for semantically similar existing memories before inserting
- Update the existing entry instead of creating a new one if similarity is high enough
- Use vector similarity when an embedder exists, otherwise use text matching

### 4.3 Expiration Management

Memory expiration has two dimensions: time-based expiration and count-based eviction.

Time-based expiration:

- Compute `expires_at` from `auto_forget_days` at creation time
- `None` means never expires
- `forget_expired()` removes entries whose `expires_at < Utc::now()`

Periodic cleanup:

- Triggered externally, such as by a cron job
- `memory` exposes cleanup methods but does not own a scheduler
- In the enhanced architecture, heartbeat or routine may also trigger cleanup through `agent`

Count-based eviction:

- `max_entries_per_user` limits the total number of memories per user
- Evict by:
  1. Lowest `importance`
  2. Oldest `last_accessed` among entries with equal importance

### 4.4 Vector Embeddings

`memory` integrates vector embedding support through `rig::embeddings::EmbeddingModel`.

- `MemoryManager` holds `Option<Box<dyn EmbeddingModel>>`
- The embedder is injected by the `agent` assembly layer
- Any rig-compatible embedding model can be used

Embedding storage:

- Dimensions depend on the model
- Stored as `Vec<f32>` in `MemoryEntry.embedding`
- SQLite can serialize it into a BLOB

Cosine similarity:

```text
cosine_similarity(a, b) = dot(a, b) / (||a|| * ||b||)
```

A similarity threshold, such as 0.7, can be used to filter unrelated memories.

---

## 5. File Structure

```text
crates/memory/src/
├── lib.rs
└── manager.rs
```

Responsibilities:

- `lib.rs`: `MemoryStore`, `MemoryEntry`, `MemoryCategory`, and public serializable types
- `manager.rs`: `MemoryManager`, including recall, automatic storage, expiration cleanup, cosine similarity, and keyword matching

---

## 6. Configuration

Example memory configuration:

```json
{
  "memory": {
    "enabled": true,
    "max_entries_per_user": 1000,
    "auto_forget_days": 90
  }
}
```

| Config | Type | Default | Description |
|--------|------|--------|------|
| `enabled` | `bool` | `true` | Whether long-term memory is enabled |
| `max_entries_per_user` | `usize` | `1000` | Maximum memory entries per user |
| `auto_forget_days` | `u32` | `90` | Automatic expiration window in days |

---

## 7. Data Flow

Lifecycle of memory:

1. **Creation**: `MemoryManager.maybe_store(session, response)` decides whether to create memory
2. **Storage**: `MemoryManager.store(entry)` optionally generates embeddings, deduplicates, and persists
3. **Recall**: `MemoryManager.recall(content)` returns memories ranked by relevance and importance
4. **Access update**: recalled memories update `last_accessed`
5. **Expiration cleanup**: `MemoryManager.forget_expired()` removes expired entries and enforces count limits
6. **Background maintenance**: heartbeat or routine tasks may trigger summarization or cleanup, but still go through Job and Trace

Position in the context:

```text
System Prompt / Soul
Memory Context
Compressed Summary
Recent Messages
Current User Message
```
