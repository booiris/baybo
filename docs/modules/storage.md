# storage - Unified Storage Trait and Implementation Layer

## Overview

The `storage` crate is the **libsql adapter**: it implements every `*Store` trait over a single libsql backend. The trait *contracts* live in the `aura-store` ports crate (see [`README.md`](README.md)), not here, and consumers import them from `aura_store` directly — `aura-storage` does **not** re-export them. What it exposes is the concrete surface: the `Store` DI bundle, the `libsql` module, and the `retry` helper. **libsql** is the sole backend.

Its job is:

- Implement every `*Store` trait from `aura-store` (`SessionStore`, `SessionSummaryStore`, `JobStore`, `TraceStore`, `MemoryStore`, `CostStore`, `SecretStore`, `CronStore`, `BlobStore`, `ChannelSessionStore`, `ChannelBotStore`, `ChannelPairingStore`, `SkillRiskStore`) via libsql
- Provide `Store` for dependency injection
- Manage database schema initialization

Because the trait contracts and their row/DTO types live in `aura-store` (a leaf over `aura-model`), `aura-storage` no longer depends on any of the domain crates whose stores it implements — its only normal dependencies are `aura-store` + `aura-model`. `aura-job` and `aura-trace` stay on as `dev-dependencies` alone, so the libsql round-trip tests can build the rich `Job` / `Step` / `Span` types and call their `to_row` / `from_row` helpers. Domain crates depend on `aura-store` to *call* a store; the assembly layer wires in `aura-storage`.

## Design Decisions

### All store traits live in the ports crate

Every `*Store` trait contract lives in `aura-store`; `aura-storage` only *implements* them. Most traits trade in plain value types (`aura-model` domain types, or row/DTO types defined alongside the trait in `aura-store`). Two of them — `JobStore` and `TraceStore` — trade in **row DTOs** (`JobRow` / `JobTransitionRow`, `StepRow` / `SpanRow` / `SpanEventRow`: a queryable key plus the serialized entity in a `data` column) so the trait can sit in the leaf ports crate while the rich `Job` / `Step` / `Span` types — which carry the state-machine and recorder logic — stay in `aura-job` / `aura-trace`. Those two crates own the `to_row` / `from_row` conversions and convert at the call boundary.

```
libsql/session.rs         → impl SessionStore + SessionSummaryStore   (traits + StoredMessage / SessionSummaryRow from aura-store)
libsql/memory.rs          → impl MemoryStore                          (trait from aura-store)
libsql/trace.rs           → impl TraceStore                           (trait from aura-store; rows ↔ Step/Span/SpanEvent via aura-trace)
libsql/secret.rs          → impl SecretStore                          (trait from aura-store)
libsql/job.rs             → impl JobStore                             (trait from aura-store; rows ↔ Job via aura-job)
libsql/cost.rs            → impl CostStore                            (trait from aura-store)
libsql/cron.rs            → impl CronStore                            (trait from aura-store; libsql adapter handles JSON serialization)
libsql/skill_risk.rs      → impl SkillRiskStore                       (trait + RiskVerdict / RiskLevel from aura-store)
libsql/channel_session.rs → impl ChannelSessionStore                  (trait from aura-store)
libsql/channel_bot.rs     → impl ChannelBotStore                      (trait + ChannelBotRow from aura-store)
libsql/channel_pairing.rs → impl ChannelPairingStore                  (trait + ChannelPairingRow / PairingStatus from aura-store)
libsql/blob.rs            → impl BlobStore                            (trait + BlobMeta from aura-store)
```

Each file above holds its store's queries, but the table DDL is not colocated: every `CREATE TABLE` lives in `libsql/mod.rs`'s schema initialization — the single place to read the full set of persisted tables or add a new one.

`Session`, `User`, `ChannelType`, and `SessionState` live in `aura-model` so that both `aura-session` (the `SessionManager` facade) and `aura-storage` (libsql impl) can type against them without either crate dragging the other along. The `SessionStore` / `SessionSummaryStore` traits and their `StoredMessage` / `SessionSummaryRow` row types live in `aura-store`; `aura-storage` implements them and `aura-session` calls them.

The conversation transcript itself is **not** stored on `Session` — it's owned by `aura_context::ContextManager` while the actor is alive and persisted via the per-message `SessionStore` log: `append_session_message` for new turns, `apply_session_compaction` for `/compact`, `load_active_session_messages` for cold-start hydration. Rows live in the `session_messages` table (append-only, with a `superseded_by` marker for compactions).

The row schema carries **no read/unread state** — there is no `read_at`, `seen`, or unread-count column, and none exists anywhere server-side. "Unread" is a purely client-side derivation: the web sidebar counts `Frame::SessionActivity` pulses (see [channels.md](channels.md)) and the cron inbox tracks acknowledged fires in `localStorage`; neither is persisted, so read status doesn't survive a cleared browser or follow the user across devices. Adding server-trusted read state would be a from-scratch change spanning storage → `aura-model` → gateway API → web.

`CostStore` and `SkillRiskStore` are unique in that their data types have no separate domain crate, so those types (`RiskVerdict` / `RiskLevel`, plus cost's `MicroUsd`-based rows) sit next to the trait in `aura-store` — that keeps `aura-skills` LLM-free while still letting the assessor crate persist verdicts against an opaque row type. `ChannelPairingStore` follows the same pattern: its row + status types (`ChannelPairingRow`, `PairingStatus`) live alongside the trait in `aura-store`, so `aura-pairing` depends on the ports crate alone rather than owning its own persistence contract.

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

All libsql-backed deletes are plain `DELETE FROM`. There is no `deleted_at` tombstone column, no soft-delete protocol, and no revival semantics — once a row is gone it is gone. The one cadence-driven retention sweep in `aura-janitor` is `channel_pairings` (expired/abandoned auth-flow rows), which issues the same `DELETE FROM` against rows past their retention horizon. Blobs are **not** swept on a TTL; the `BlobStore::purge_older_than` capability still exists but is no longer wired to the janitor, so a blob row lives until an explicit `BlobStore::delete` removes it (which unlinks the content-addressed payload once no live row still references it).

## Constraints

- Normal dependencies are just `aura-store` (trait contracts + row/DTO types) and `aura-model` (domain types) — no domain crate; reverse edges from any domain crate back to `aura-storage` do not exist. `aura-job` / `aura-trace` are `dev-dependencies` only (round-trip tests)
- Exposes trait objects externally, not concrete backend types
- Assumes upper layers have already sanitized data before persistence

## Collaboration

| Module                                   | Role                                                                                      |
| ---------------------------------------- | ----------------------------------------------------------------------------------------- |
| `storage` (self)                         | Provides libsql implementations for every Store trait; defines the channel / pairing / cron / risk / blob trait surface |
| `store`                                  | Owns every `*Store` trait contract + its row/DTO types; `storage` implements them and depends only on this crate (+ `model`) |
| `model` / `trace` / `job`                | Provide domain types the libsql impls round-trip (`trace` / `job` are `dev-dependencies` only, for the round-trip tests) |
| `context`                                | Owns `ContextManager`; pure in-memory                                                     |
| `session`                                | Owns the `SessionManager` facade and calls `SessionStore` / `SessionSummaryStore` (whose traits live in `aura-store`); `storage` does **not** depend on `session` |
| `agent`                                  | Injects stores into managers (MemoryManager, JobLifecycle, etc.); re-exports SessionManager |
