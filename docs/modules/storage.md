# storage - Unified Storage Trait and Implementation Layer

## Overview

The `storage` crate hosts libsql implementations for every Store trait in the workspace plus the remaining trait definitions whose domain doesn't have its own crate. Domain crates that do exist own their own trait surface: `SessionStore` / `SessionSummaryStore` in `aura-session`, `JobStore` in `aura-job`, `TraceStore` in `aura-trace`, `MemoryStore` in `aura-memory`, `CostStore` in `aura-cost`, `SecretStore` in `aura-security`. **libsql** is the sole backend.

Its job is:

- Define the remaining Store traits whose domain has no dedicated crate (`channel_session`, `channel_bot`, `channel_pairing`, `cron`, `skill_risk`, `blob`)
- Implement every Store trait — including those owned by domain crates — via libsql
- Provide `Store` for dependency injection
- Manage database schema initialization

Domain crates own their full persistence vertical (trait + manager + test-support fake). `aura-storage` depends on each one for the trait it must implement.

## Design Decisions

### Trait location follows domain ownership

Each domain crate (`session`, `job`, `trace`, `memory`, `cost`, `security`, `cron`) owns its own trait. `aura-storage` implements them. The remaining trait definitions — `SkillRiskStore`, `ChannelSessionStore`, `ChannelBotStore`, `ChannelPairingStore`, `BlobStore` — live in `aura-storage` because their consumer crates either don't exist (`channel_*`, `blob`) or deliberately keep persistence out of their dependency graph (`skills-assessor` consumes opaque row types).

```
libsql/session.rs         → impl SessionStore + SessionSummaryStore   (traits from aura-session)
libsql/memory.rs          → impl MemoryStore                          (trait from aura-memory)
libsql/trace.rs           → impl TraceStore                           (trait from aura-trace)
libsql/secret.rs          → impl SecretStore                          (trait from aura-security)
libsql/job.rs             → impl JobStore                             (trait from aura-job)
libsql/cost.rs            → impl CostStore                            (trait from aura-cost)
libsql/cron.rs            → impl CronStore                            (trait from aura-cron; libsql adapter handles JSON serialization)
libsql/skill_risk.rs      → impl SkillRiskStore                       (trait + RiskVerdict / RiskLevel here)
libsql/channel_session.rs → impl ChannelSessionStore                  (trait here)
libsql/channel_bot.rs     → impl ChannelBotStore                      (trait here)
libsql/channel_pairing.rs → impl ChannelPairingStore                  (trait + ChannelPairingRow / PairingStatus here)
libsql/blob.rs            → impl BlobStore                            (trait + BlobMeta here)
```

`Session`, `User`, `ChannelType`, and `SessionState` live in `aura-model` so that both `aura-session` (trait + manager) and `aura-storage` (libsql impl) can type against them without either crate dragging the other along. The `SessionStore` / `SessionSummaryStore` traits themselves now live in `aura-session`; `aura-storage` depends on `aura-session` to implement them.

The conversation transcript itself is **not** stored on `Session` — it's owned by `aura_context::ContextManager` while the actor is alive and persisted via the per-message `SessionStore` log: `append_session_message` for new turns, `apply_session_compaction` for `/compact`, `load_active_session_messages` for cold-start hydration. Rows live in the `session_messages` table (append-only, with a `superseded_by` marker for compactions).

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

- Depends on every domain crate whose trait it implements (`model`, `session`, `trace`, `security`, `job`, `memory`, `cost`); reverse edges from those crates back to `aura-storage` do not exist
- Exposes trait objects externally, not concrete backend types
- Assumes upper layers have already sanitized data before persistence

## Collaboration

| Module                                   | Role                                                                                      |
| ---------------------------------------- | ----------------------------------------------------------------------------------------- |
| `storage` (self)                         | Provides libsql implementations for every Store trait; defines the channel / pairing / cron / risk / blob trait surface |
| `model` / `trace` / `security` / `job` / `memory` / `cost` / `session` | Provide domain types and store traits consumed by the libsql impls         |
| `context`                                | Owns `ContextManager`; pure in-memory                                                     |
| `session`                                | Owns `SessionStore` / `SessionSummaryStore` traits + `SessionManager`; `storage` depends on `session` |
| `agent`                                  | Injects stores into managers (MemoryManager, JobLifecycle, etc.); re-exports SessionManager |
