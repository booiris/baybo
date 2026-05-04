# storage - Unified Storage Trait and Implementation Layer

## Overview

The `storage` crate is the single source of truth for all persistence interfaces and implementations. It defines **all** Store traits (`SessionStore`, `MemoryStore`, `TraceStore`, `SecretStore`, `JobStore`, `CostStore`, `CronStore`, `SkillRiskStore`, `ChannelSessionStore`, `ChannelBotStore`, `ChannelPairingStore`) and implements them via **libsql** as the sole backend.

Its job is:

- Define all Store traits (each in its own submodule: `session`, `memory`, `trace`, `secret`, `job`, `cost`, `cron`, `risk`, `channel_session`, `channel_bot`, `channel_pairing`)
- Implement all Store traits via libsql
- Provide `Store` for dependency injection
- Manage database schema initialization

Domain crates (`model`, `trace`, `security`, `job`) provide only **types**. Business logic (managers, collectors, gateways) lives in `agent` or — for `SessionManager` — in `aura-session`, which depends on `aura-storage` for the `SessionStore` trait.

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

`Session`, `User`, `ChannelType`, and `SessionState` live in `aura-model` (not `aura-session`) so that `storage` can type `SessionStore` on `aura_model::Session` without pulling in `aura-session`. That keeps `aura-session` free to depend on `aura-storage` for the trait it consumes, avoiding a cycle via `storage → trace → context → session` and `storage → security → channels → session`.

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

Fields difficult to fully structure (`SessionState.extra`, `Job.input/output`) are stored as JSON. The trace stack uses a hybrid columnar schema: `steps` and `spans` keep queryable columns (`kind`, `started_at`, `ended_at`, `outcome`, `job_id`, `step_id`, `parallel_group`) plus a JSON `data` blob for full round-trip; `span_events` keeps `step_id`, `span_id`, `kind`, `at` columnar with JSON payload. The security requirement still applies: values must already be sanitized before persistence.

### Transaction boundaries

Use transactions wherever a check-and-write pair must be atomic — most importantly `SessionStore::soft_delete`, which checks for live forks before stamping the parent's `deleted_at` (a non-transactional implementation admits orphan forks under concurrent `create_fork`).

### Soft delete (libsql)

All libsql-backed tables that support deletion use **soft delete**, never a hard `DELETE`. This preserves history for audit, replay, and compliance.

- Every deletable table carries a nullable `deleted_at INTEGER` column (**Unix µs**, written via `super::time::now_us()`; `NULL` = live row).
- Deletion = `UPDATE ... SET deleted_at = ?now WHERE ... AND deleted_at IS NULL`. Do not emit `DELETE FROM` against these tables.
- Every read (`SELECT`) MUST include `AND deleted_at IS NULL` so soft-deleted rows stay hidden. Every mutation (`UPDATE`) on a live row MUST include the same guard so you never write through a deleted row.
- Re-insertion semantics: `INSERT OR REPLACE` and `ON CONFLICT ... DO UPDATE` must reset `deleted_at` back to `NULL` so recreating a soft-deleted id revives it (see `skill_risk.rs::upsert_job` for the pattern).
- Schema changes: add the column to the `CREATE TABLE IF NOT EXISTS` in `crates/storage/src/libsql/mod.rs`.
- Tables currently covered: `sessions`, `memories`, `secrets`, `jobs`, `job_transitions`, `steps`, `spans`, `span_events`, `cron_jobs`, `cron_executions`, `skill_risk_assessments`, `skill_risk_assessment_jobs`, `channel_sessions`, `channel_bots`, `channel_pairings`, `blobs`, `user_monthly_cost`. The only append-only table without `deleted_at` is `cost_records` (billing audit trail).
- **Retention exceptions.** Two tables expose hard-delete retention sweeps that bypass the protocol — they keep `deleted_at` for the live-vs-tombstone distinction during normal operation but `aura-janitor` issues `DELETE FROM` on rows older than the retention horizon. Documented exceptions, not bugs:
  - `cron_executions` — `purge_completed_executions_older_than(cutoff)` hard-deletes non-`pending` rows. Audit trail beyond the horizon is not preserved.
  - `channel_pairings` — `purge_expired(now)` hard-deletes rows whose `expires_at < now`. Pairing codes are short-lived and intentionally non-recoverable past expiry.

## Constraints

- Depends on domain crates for types only (`model`, `trace`, `security`, `job`) — not on `aura-session`, to keep `aura-session → aura-storage` acyclic
- Exposes trait objects externally, not concrete backend types
- Assumes upper layers have already sanitized data before persistence

## Collaboration

| Module                                   | Role                                                                                      |
| ---------------------------------------- | ----------------------------------------------------------------------------------------- |
| `storage` (self)                         | Defines all Store traits; provides all libsql implementations; defines cost / risk types  |
| `model` / `trace` / `security` / `job`   | Provide domain types consumed by Store traits                                             |
| `session`                                | Owns `SessionManager`; depends on `storage` to consume `SessionStore`                     |
| `agent`                                  | Injects stores into managers (MemoryManager, JobManager, etc.); re-exports SessionManager |
