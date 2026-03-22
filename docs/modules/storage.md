# storage - Unified Storage Implementation Layer

## 1. Module Overview

The `storage` crate implements the Store traits exposed by other modules, such as `SessionStore`, `TraceStore`, `SecretStore`, and `JobStore`.

Its job is not to define domain models, but to:

- Provide a unified backend factory
- Offer separate persistence implementations for different domains
- Isolate SQLite and in-memory implementation details

Under the architecture constraints, `storage` should not use one giant struct to implement every interface. Each domain should have an independent Store implementation.

---

## 2. Dependencies

### 2.1 Internal Dependencies

`storage` depends on all crates that define Store traits:

- `session`
- `memory`
- `trace`
- `security`
- `cost`
- `job`

As well as the shared type crate `core`.

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `sqlx` / `rusqlite` | SQLite implementation |
| `tokio` | Async database access |
| `serde_json` | Structured-field serialization |
| `anyhow` | Error handling |

---

## 3. Public Interfaces

### 3.1 StorageFactory

```rust
pub struct StorageFactory;

impl StorageFactory {
    pub fn create(config: &StorageConfig) -> Result<StorageSet> {
        match config.backend {
            Backend::Memory => Ok(StorageSet::in_memory()),
            Backend::Sqlite => Ok(StorageSet::sqlite(&config.sqlite_path)?),
        }
    }
}
```

### 3.2 StorageSet

`StorageSet` is the container used by the assembly layer when injecting dependencies.

```rust
pub struct StorageSet {
    pub session: Box<dyn SessionStore>,
    pub memory: Box<dyn MemoryStore>,
    pub trace: Box<dyn TraceStore>,
    pub secret: Box<dyn SecretStore>,
    pub cost: Box<dyn CostStore>,
    pub job: Box<dyn JobStore>,
}
```

### 3.3 SQLite Implementation Types

```rust
pub struct SqliteSessionStore { pool: SqlitePool }
pub struct SqliteMemoryStore  { pool: SqlitePool }
pub struct SqliteTraceStore   { pool: SqlitePool }
pub struct SqliteSecretStore  { pool: SqlitePool }
pub struct SqliteCostStore    { pool: SqlitePool }
pub struct SqliteJobStore     { pool: SqlitePool }
```

Each struct implements only one trait, in line with the interface segregation principle.

---

## 4. Implementation Details

### 4.1 The Role of the Factory Pattern

`StorageFactory` solves initialization complexity, not domain abstraction. At application startup:

```rust
let stores = StorageFactory::create(&config.storage)?;
```

Then inject `stores.session`, `stores.trace`, and so on into the corresponding modules.

### 4.2 Reusing the SQLite Connection Pool

Although each Store is an independent struct, they can share one `SqlitePool`:

```rust
let pool = SqlitePool::connect(path).await?;

SqliteSessionStore { pool: pool.clone() }
SqliteTraceStore   { pool: pool.clone() }
...
```

This preserves domain isolation while avoiding repeated connection setup.

### 4.3 Table Design Recommendations

It is recommended that each Store manage its own tables rather than defining cross-domain aggregate tables in `storage`.

Examples:

- `sessions`
- `memories`
- `traces`
- `trace_nodes`
- `secrets`
- `cost_records`
- `jobs`
- `job_transitions`

### 4.4 JSON Field Strategy

Fields that are difficult to fully structure may be stored as JSON:

- `SessionState.extra`
- `MessageMetadata.extra`
- `Job.input/output`
- `TraceSpan.input/result`

The security requirement still applies: these JSON values must already be sanitized.

### 4.5 Positioning of the In-Memory Backend

`memory_backend` is mainly for:

- Unit tests
- Local development
- Temporary runs that do not require persistence

It should not promise data retention across process restarts, nor should it try to simulate full transaction semantics.

### 4.6 Recommended Transaction Boundaries

Use transactions for scenarios such as:

- `JobStore.update_status()` writing both `jobs` and `job_transitions`
- `TraceStore.save_trace()` writing both the trace root and node tables
- `SecretStore.store()` when replacing an old value

---

## 5. Collaboration with Other Modules

| Module | Collaboration |
|------|---------|
| `session` | Persists sessions |
| `memory` | Persists long-term memory |
| `trace` | Persists full trace trees and snapshots |
| `security` | Persists encrypted secrets |
| `cost` | Persists cost records |
| `job` | Persists jobs and state transitions |
| `agent` | Injects all implementations through `StorageSet` |

---

## 6. Implementation Recommendations

- Keep SQLite schema migrations and Store implementations under the same module directory
- Expose trait objects externally instead of concrete SQLite types
- Assume upper layers have already sanitized data before persistence, but critical tables may still perform defensive secondary checks
