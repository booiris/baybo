# storage - Unified Storage Trait and Implementation Layer

## Overview

The `storage` crate is the single source of truth for all persistence interfaces and implementations. It defines **all** Store traits (`SessionStore`, `MemoryStore`, `TraceStore`, `SecretStore`, `JobStore`, `CostStore`, `CronStore`, `SkillRiskStore`, `ChannelSessionStore`, `ChannelBotStore`, `ChannelPairingStore`) and implements them via **libsql** as the sole backend.

Its job is:

- Define all Store traits (each in its own submodule: `session`, `memory`, `trace`, `secret`, `job`, `cost`, `cron`, `risk`, `channel_session`, `channel_bot`, `channel_pairing`)
- Implement all Store traits via libsql
- Provide `Store` for dependency injection
- Manage database schema initialization

Domain crates (`model`, `trace`, `security`, `job`) provide only **types**. Business logic (managers, collectors, gateways) lives in `agent` or — for `SessionManager` — in `aura-context`, which depends on `aura-storage` for the `SessionStore` trait.

## Design Decisions

### All Store traits defined in storage

Every Store trait lives in `storage`, not in the domain crate. This avoids circular dependencies: domain crates define types → `storage` depends on those types to define traits → managers depend on both to wire business logic. All Store traits use `StorageError` as their error type — domain-specific error types do not leak into storage.

```
session.rs         → SessionStore         (uses aura_model session types)
memory.rs          → MemoryStore          (uses aura_model memory types)
trace.rs           → TraceStore           (uses aura_trace types)
secret.rs          → SecretStore          (uses aura_security types)
job.rs             → JobStore             (uses aura_job types)
cost.rs            → CostStore            (defines its own types: CostRecord, CostSummary, TimeRange)
cron.rs            → CronStore            (opaque row types: CronJobRow, CronExecutionRow — no dep on aura_cron)
risk.rs            → SkillRiskStore       (defines RiskVerdict, RiskLevel, AssessmentJob, AssessmentJobStatus — consumed by aura-skills-assessor)
channel_session.rs → ChannelSessionStore  (maps (channel_type, user_id) → aura session_id for sidecars)
channel_bot.rs     → ChannelBotStore      (per-tenant bot metadata; token lives in the vault)
channel_pairing.rs → ChannelPairingStore  (defines ChannelPairingRow, PairingStatus — consumed by aura-pairing)
```

`Session`, `User`, `ChannelType`, and `SessionState` live in `aura-model` (not `aura-context`) so that `storage` can type `SessionStore` on `aura_model::Session` without pulling in `aura-context`. That keeps `aura-context` free to depend on `aura-storage` for the trait it consumes.

The conversation transcript itself is **not** stored on `Session` — it's owned by `aura_context::ContextManager` while the actor is alive and persisted via `SessionStore::save_context_messages` / `load_context_messages` to a `context_messages` JSON column on the `sessions` table. The agent loop flushes after every successful `run` and `compact_now`; the router seeds it from the same column on cold start.

`CostStore` and `SkillRiskStore` are unique in that they also define their own data types: cost has no separate domain crate, and risk types live in storage to keep `aura-skills` LLM-free while still allowing the assessor crate to persist verdicts. `ChannelPairingStore` follows the same pattern: its row + status types (`ChannelPairingRow`, `PairingStatus`) sit next to the trait so `aura-pairing` can depend on `aura-storage` alone rather than owning its own persistence contract.

`SkillRiskStore` persists two kinds of rows:

- `skill_risk_assessments` — finalized `RiskVerdict`s, keyed by `(skill_name, content_hash)`. The content hash's prefix tag distinguishes full-scope from primary-scope verdicts, so one table serves both scopes without an extra column.
- `skill_risk_assessment_jobs` — in-flight full-scope assessments enqueued for the background worker (`AssessmentJob { skill_name, content_hash, source_path, status, attempts, last_error, created_at, updated_at }`, status one of `Pending`/`InProgress`/`Failed`). Written _before_ the channel send so a crash between persist and send is recoverable; `load_pending_jobs()` re-enqueues survivors on startup. `forget(skill)` deletes from both tables so a removed skill doesn't leave orphan work behind.

### Single backend: libsql

All store implementations use libsql (async-native, SQLite-compatible). There is no rusqlite or separate in-memory backend. `Store::open(path)` opens (or creates) a file-backed libsql database (creating parent directories if missing); `LibsqlPool::open_in_memory()` is still available for tests. `LibsqlPool` wraps a shared `libsql::Connection` behind `Arc` for cheap cloning across async tasks.

The database file path is not a user-facing config knob. Bootstrap composes it from the project root via `boot::storage_db_path()` — storage always lives at `<workspace.path>/state/storage.db`. Operators pick the project root; the storage layout underneath it is fixed by convention.

### One struct per trait

Each domain has an independent Store implementation (`LibsqlSessionStore`, `LibsqlCostStore`, etc.). All share one `LibsqlPool` while preserving domain isolation. No giant struct implementing every interface.

### Store solves initialization, not abstraction

At startup: `Store::open(path)` creates a `LibsqlPool` and wires all store implementations. The `agent` layer injects individual stores into corresponding managers.

### Each Store manages its own queries

Table schemas are created centrally in `LibsqlPool::init_db()`, but each Store struct owns its own query logic. No cross-domain aggregate tables.

### JSON field strategy

Fields difficult to fully structure (`SessionState.extra`, `Job.input/output`) are stored as JSON. The trace stack stores the entire entity as a JSON `data` blob; queryable fields (`job_id`, `step_id`, `started_at`, `ended_at`) surface as `GENERATED ALWAYS AS (json_extract(...)) VIRTUAL` columns SQLite keeps in lockstep with `data` automatically — no two-side write contract for the storage layer to enforce. The security requirement still applies: values must already be sanitized before persistence.

### Transaction boundaries

Use transactions wherever a check-and-write pair must be atomic — most importantly `SessionStore::delete`, which scans for live forks before removing the parent row (a non-transactional implementation admits orphan forks under concurrent `create_fork`).

### Hard delete

All libsql-backed deletes are plain `DELETE FROM`. There is no `deleted_at` tombstone column, no soft-delete protocol, and no revival semantics — once a row is gone it is gone. Retention sweeps in `aura-janitor` (`cron_executions`, `cost_records`, `channel_pairings`, `blobs`) issue the same `DELETE FROM` against rows past their retention horizon.

## Constraints

- Depends on domain crates for types only (`model`, `trace`, `security`, `job`) — not on `aura-context`, to keep `aura-context → aura-storage` acyclic
- Exposes trait objects externally, not concrete backend types
- Assumes upper layers have already sanitized data before persistence

## Collaboration

| Module                                   | Role                                                                                      |
| ---------------------------------------- | ----------------------------------------------------------------------------------------- |
| `storage` (self)                         | Defines all Store traits; provides all libsql implementations; defines cost / risk types  |
| `model` / `trace` / `security` / `job`   | Provide domain types consumed by Store traits                                             |
| `context`                                | Owns `ContextManager` and `SessionManager`; depends on `storage` to consume `SessionStore`|
| `agent`                                  | Injects stores into managers (MemoryManager, JobManager, etc.); re-exports SessionManager |
