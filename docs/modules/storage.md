# storage - Unified Storage Trait and Implementation Layer

## Overview

The `storage` crate is the single source of truth for all persistence interfaces and implementations. It defines **all** Store traits (`SessionStore`, `MemoryStore`, `TraceStore`, `SecretStore`, `JobStore`, `CostStore`) and implements them via **libsql** as the sole backend.

Its job is:

- Define all Store traits (each in its own submodule: `session`, `memory`, `trace`, `secret`, `job`, `cost`)
- Implement all Store traits via libsql
- Provide `Store` for dependency injection
- Manage database schema initialization

Domain crates (`model`, `session`, `trace`, `security`, `job`) provide only **types**. Business logic (managers, collectors, gateways) lives in `agent`.

## Design Decisions

### All Store traits defined in storage

Every Store trait lives in `storage`, not in the domain crate. This avoids circular dependencies: domain crates define types → `storage` depends on those types to define traits → `agent` depends on both to wire business logic. All Store traits use `StorageError` as their error type — domain-specific error types do not leak into storage.

```
session.rs  → SessionStore  (uses aura_session types)
memory.rs   → MemoryStore   (uses aura_model memory types)
trace.rs    → TraceStore    (uses aura_trace types)
secret.rs   → SecretStore   (uses aura_security types)
job.rs      → JobStore      (uses aura_job types)
cost.rs     → CostStore     (defines its own types: CostRecord, CostSummary, TimeRange)
```

`CostStore` is unique in that it also defines its own data types, because cost has no separate domain crate.

### Single backend: libsql

All store implementations use libsql (async-native, SQLite-compatible). There is no rusqlite or separate in-memory backend. `Store::open(path)` opens (or creates) a file-backed libsql database (creating parent directories if missing); `LibsqlPool::open_in_memory()` is still available for tests. `LibsqlPool` wraps a shared `libsql::Connection` behind `Arc` for cheap cloning across async tasks.

The database file path is not a user-facing config knob. Bootstrap composes it from the project root via `boot::storage_db_path()` — storage always lives at `<workspace.path>/.aura/storage.db`. Operators pick the project root; the storage layout underneath it is fixed by convention.

### One struct per trait

Each domain has an independent Store implementation (`LibsqlSessionStore`, `LibsqlCostStore`, etc.). All share one `LibsqlPool` while preserving domain isolation. No giant struct implementing every interface.

### Store solves initialization, not abstraction

At startup: `Store::open(path)` creates a `LibsqlPool` and wires all store implementations. The `agent` layer injects individual stores into corresponding managers.

### Each Store manages its own queries

Table schemas are created centrally in `LibsqlPool::init_db()`, but each Store struct owns its own query logic. No cross-domain aggregate tables.

### JSON field strategy

Fields difficult to fully structure (`SessionState.extra`, `Job.input/output`, `TraceSpan.input/result`) are stored as JSON. The security requirement still applies: these values must already be sanitized.

### Transaction boundaries

Use transactions for: `TraceStore.save_trace()` writing trace root and nodes atomically.

## Constraints

- Depends on domain crates for types only (`model`, `session`, `trace`, `security`, `job`)
- Exposes trait objects externally, not concrete backend types
- Assumes upper layers have already sanitized data before persistence

## Collaboration

| Module                                               | Role                                                                                         |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `storage` (self)                                     | Defines all Store traits; provides all libsql implementations; defines cost types             |
| `model` / `session` / `trace` / `security` / `job`   | Provide domain types consumed by Store traits                                                 |
| `agent`                                              | Injects stores into managers; owns all business logic (SessionManager, MemoryManager, etc.)   |
