# storage - Unified Storage Trait and Implementation Layer

## Overview

The `storage` crate is the **sqlite adapter**: it implements every `*Store` trait over a single sqlite backend. The trait *contracts* live in the `baybo-store` ports crate (see [`README.md`](README.md)), not here, and consumers import them from `baybo_store` directly — `baybo-storage` does **not** re-export them. What it exposes is the concrete surface: the `Store` DI bundle, the `sqlite` module, and the `retry` helper. **sqlite** is the sole backend.

Its job is:

- Implement every `*Store` trait from `baybo-store` (`SessionStore`,
  `SessionSummaryStore`, `SessionFolderStore`, `TaskStore`, `JobStore`,
  `TraceStore`, `CostStore`, `SecretStore`, `CronStore`, `BlobStore`,
  `ChannelSessionStore`, `ChannelBotStore`, `ChannelPairingStore`,
  `DeviceStore`, `SkillRiskStore`, `AgentProfileStore`,
  `DeckCardStore`) via sqlite
- Provide `Store` for dependency injection
- Manage database schema initialization

Because the trait contracts and their row/DTO types live in `baybo-store` (a leaf over `baybo-model`), `baybo-storage` no longer depends on any of the domain crates whose stores it implements — its only normal dependencies are `baybo-store` + `baybo-model`. `baybo-job` and `baybo-trace` stay on as `dev-dependencies` alone, so the sqlite round-trip tests can build the rich `Job` / `Step` / `Span` types and call their `to_row` / `from_row` helpers. Domain crates depend on `baybo-store` to *call* a store; the assembly layer wires in `baybo-storage`.

## Design Decisions

### One connection per in-flight operation — a memory-safety rule, not a tuning knob

`SqlitePool` (`sqlite/mod.rs`) is a real pool of `rusqlite::Connection`s (`deadpool-sqlite`, `POOL_SIZE = 8`). Stores never hold a connection; they reach the database only through:

```rust
pool.interact("sessions.load_last_user_message", move |conn| { … }).await
```

which checks a connection out **exclusively for the whole closure** — prepare, step, *and every `row.get()`*.

The exclusivity is load-bearing, and the reason is not throughput. A sqlite connection owns an unsynchronised private heap — its lookaside allocator — and the C API's own accessors mutate it: `sqlite3_value_text()` allocates in order to NUL-terminate a TEXT column. **The decode is as much a critical section as the query is**, so a lock around only the statement is a non-fix. Two threads inside one connection corrupt the lookaside free list, and the process dies later in `sqlite3DbMallocRawNN` when something pops the dangling head — far from the code that broke it.

What makes the rule hold is the type system, not vigilance: `rusqlite::Connection` is `Send` but deliberately **not** `Sync`, so `Arc<Connection>` shared across tasks does not compile. `crates/storage/tests/concurrency.rs` is the regression guard; a driver that permits the aliasing fails it with a signal, not an assertion.

Consequences worth knowing:

- **The closure is `'static`.** Bind every parameter as an owned value (`session_id.as_str().to_string()`), and do fallible decoding (serde, chrono) in the outer `anyhow` closure rather than inside a `query_map` row closure, which may only yield `rusqlite::Result`.
- **No `.await` inside a closure.** A method that must await something non-SQL between statements (`sqlite/blob.rs` does, for path locks and filesystem writes) splits into several `interact` calls. That is behaviour-preserving only where the statements were not already one transaction — check before splitting.
- **`busy_timeout` is 5s, not 0.** With a single shared connection, intra-process write contention was impossible: everything queued behind one handle. A pool makes concurrent writers real, so without a timeout the agent loop and the trace sink would trade spurious `SQLITE_BUSY`. [`retry`](../../crates/storage/src/retry.rs) is now the *second* line of defence, for cross-process contention (the CLI writing the same file as a running gateway) that outlives the timeout.
- **Transactions get exclusivity for free.** A `BEGIN IMMEDIATE` block runs inside one closure on one connection, so another task's statements can no longer land inside it — which they could when every task shared the one handle.

### All store traits live in the ports crate

Every `*Store` trait contract lives in `baybo-store`; `baybo-storage` only *implements* them. Most traits trade in plain value types (`baybo-model` domain types, or row/DTO types defined alongside the trait in `baybo-store`). Two of them — `JobStore` and `TraceStore` — trade in **row DTOs** (`JobRow` / `JobTransitionRow`, `StepRow` / `SpanRow` / `SpanEventRow`: a queryable key plus the serialized entity in a `data` column) so the trait can sit in the leaf ports crate while the rich `Job` / `Step` / `Span` types — which carry the state-machine and recorder logic — stay in `baybo-job` / `baybo-trace`. Those two crates own the `to_row` / `from_row` conversions and convert at the call boundary. `TraceStore::list_unfinished_steps` is the recovery-oriented indexed query: it returns steps that are themselves open or have an open child span, including detached steps under terminal jobs.

```
sqlite/session.rs         → impl SessionStore                         (trait + StoredMessage from baybo-store)
sqlite/session_summary.rs → impl SessionSummaryStore                  (trait + SessionSummaryRow from baybo-store)
sqlite/trace.rs           → impl TraceStore                           (trait from baybo-store; rows ↔ Step/Span/SpanEvent via baybo-trace)
sqlite/secret.rs          → impl SecretStore                          (trait from baybo-store; one secrets table shared by minted placeholders, mcp.* creds, and user_env.* user secrets)
sqlite/job.rs             → impl JobStore                             (trait from baybo-store; rows ↔ Job via baybo-job)
sqlite/cost.rs            → impl CostStore                            (trait from baybo-store)
sqlite/cron.rs            → impl CronStore                            (trait from baybo-store; sqlite adapter handles JSON serialization)
sqlite/skill_risk.rs      → impl SkillRiskStore                       (trait + RiskVerdict / RiskLevel from baybo-store)
sqlite/channel_session.rs → impl ChannelSessionStore                  (trait from baybo-store)
sqlite/channel_bot.rs     → impl ChannelBotStore                      (trait + ChannelBotRow from baybo-store)
sqlite/channel_pairing.rs → impl ChannelPairingStore                  (trait + ChannelPairingRow / PairingStatus from baybo-store)
sqlite/blob.rs            → impl BlobStore                            (trait + BlobMeta from baybo-store)
sqlite/session_folder.rs  → impl SessionFolderStore                   (trait + SessionFolderRow from baybo-store)
sqlite/task.rs            → impl TaskStore                            (trait + TaskPatch from baybo-store)
sqlite/device.rs          → impl DeviceStore                          (trait + DeviceRow / DeviceStatus from baybo-store)
sqlite/agent_profile.rs   → impl AgentProfileStore                    (trait + AgentProfileRow / AgentProfileUpdate from baybo-store)
sqlite/deck.rs            → impl DeckCardStore                        (trait + DeckCardRow / DeckSnapshotRow / DeckLayoutEntry / DeckSize from baybo-store; deck_cards + latest-N-pruned deck_snapshots)
```

Each file above holds its store's queries, but the table DDL is not colocated: every `CREATE TABLE` lives in `sqlite/mod.rs`'s schema initialization — the single place to read the full set of persisted tables or add a new one.

`Session`, `User`, `ChannelType`, and `SessionState` live in `baybo-model` so that both `baybo-session` (the `SessionManager` facade) and `baybo-storage` (sqlite impl) can type against them without either crate dragging the other along. The `SessionStore` / `SessionSummaryStore` traits and their `StoredMessage` / `SessionSummaryRow` row types live in `baybo-store`; `baybo-storage` implements them and `baybo-session` calls them.

The conversation transcript itself is **not** stored on `Session` — it's owned by `baybo_context::ContextManager` while the actor is alive and persisted via the per-message `SessionStore` log: `append_session_message` for ordinary turns, `append_session_message_idempotent` for replayable source events, `apply_session_compaction` for `/compact`, and `load_active_session_messages` for cold-start hydration. Rows live in the `session_messages` table (append-only, with a `superseded_by` marker for compactions).

Read/unread state is **server-side at session granularity**: a `read_cursor` flat column on `sessions` holds the highest `session_messages.ordinal` a viewer has read. It is advanced max-wins (a lower ordinal is a no-op) by `PUT /v1/chat/sessions/:id/read` via `SessionStore::set_read_cursor` — the same targeted-UPDATE anti-clobber discipline as `hidden` / `last_llm` — and the chat list endpoint derives each row's `unread_count` from it. The web sidebar seeds its badge from that server-computed `unread_count` and bumps it live on `Frame::SessionActivity` pulses (see [channels.md](channels.md)). Cron fires need nothing extra: a recurring fire's conversation and a one-shot's notification are both ordinary rows in an ordinary session, so they accrue unread counts like any other reply.

`SkillRiskStore` is unique in that its data types have no separate domain crate, so those types (`RiskVerdict` / `RiskLevel`) sit next to the trait in `baybo-store` — that keeps `baybo-skills` LLM-free while still letting the assessor crate persist verdicts against an opaque row type. (`CostStore` needs no row DTOs at all: it trades in plain `baybo-model` types — `CostRecord` / `CostSummary` and `MicroUsd`.) `ChannelPairingStore` follows the same pattern as `SkillRiskStore`: its row + status types (`ChannelPairingRow`, `PairingStatus`) live alongside the trait in `baybo-store`, so `baybo-pairing` depends on the ports crate alone rather than owning its own persistence contract.

`SkillRiskStore` persists two kinds of rows:

- `skill_risk_assessments` — finalized `RiskVerdict`s, keyed by `(skill_name, content_hash)`. The content hash's prefix tag distinguishes full-scope from primary-scope verdicts, so one table serves both scopes without an extra column.
- `skill_risk_assessment_jobs` — in-flight full-scope assessments enqueued for the background worker (`AssessmentJob { skill_name, content_hash, source_path, status, attempts, last_error, created_at, updated_at }`, status one of `Pending`/`InProgress`/`Failed`). Written _before_ the channel send so a crash between persist and send is recoverable; `load_pending_jobs()` re-enqueues survivors on startup. `forget(skill)` deletes from both tables so a removed skill doesn't leave orphan work behind.

### Session transcript: the append-only ordinal log

The conversation transcript lives in `session_messages` as a per-session
**append-only log**, never an in-place mutable list. Each row is keyed by
`(session_id, ordinal)`, where `ordinal` is a dense, monotonic, per-session
sequence assigned at append time (`MAX(ordinal) + 1`). Rows are never deleted
or rewritten — this is user-facing core data (see the never-delete rule in the
repo `CLAUDE.md`). Columns: `role`, `content` (serialized `ContentBlock`s),
`created_at`, `source` (`MessageSource`: `user` / `cron` / `agent` — tells a
genuine prompt and a cron fire apart from the agent's own injected `user`-role
rows), `platform_msg_id` (client send idempotency key — sync-redelivery dedup, optimistic-row reconciliation, and the outbox durability point lookup), `source_event_id` (nullable durable idempotency key for internal replayable events), and `superseded_by`.

`source_event_id` is unique per session across the **full** transcript, including
superseded rows. `append_session_message_idempotent` performs the row insert and
key claim in one statement, returning `Inserted { ordinal }` or the original
`Existing { ordinal }`. Cron origin delivery uses a `cron-execution:` key;
background notification prompts, retry cues, and compaction re-anchors use
operation-scoped `background-notification:` keys. Ordinary rows leave the
column `NULL` and are unaffected by the unique partial index.

**The ordinal is the load-bearing primitive.** Because it is stable, dense, and
durable, other subsystems reference a transcript position *by value* (one `i64`)
instead of copying message content:

- `session_summaries.cursor` is the ordinal high-water mark of the last
  successful summary pass.
- trace `LlmCallInputs::Persisted { last_ordinal, .. }` records the slice an LLM
  call saw by ordinal rather than inlining it — keeps span storage constant per
  call instead of cloning a growing prefix every turn. See [trace.md](trace.md).

**Compaction supersedes, it never deletes.** `/compact` and background
summarisation both go through `apply_session_compaction(session, new_active)`,
which in one transaction (a) bulk-marks every currently-active row
`superseded_by = <first new ordinal>`, then (b) appends `new_active` at the next
contiguous ordinals. The pre-compaction rows stay in the table forever (the full
transcript is always recoverable); only their `superseded_by` flips from NULL to
the summary's ordinal.

**Two derived views over the one log:**

- **Active set** — rows where `superseded_by IS NULL`, ordered by `ordinal`:
  the live LLM context. Served by `load_active_session_messages`; a partial index
  `idx_session_messages_active` on `(session_id, ordinal) WHERE superseded_by IS
  NULL` makes it a back-of-index walk, never a full scan.
- **Full history** — every row ever appended, ignoring `superseded_by`.

**"Active as of ordinal N"** is the reconstruction that lets a stored ordinal
recover the exact active slice that *was* live when `N` was the head, even after
later compactions supersede those rows:

```
WHERE ordinal <= N AND (superseded_by IS NULL OR superseded_by > N)
```

A row superseded by a *later* compaction (`superseded_by > N`) was still part of
the snapshot at `N`; a row superseded at or before `N` was not. This filter is
what makes the ordinal references above replay-stable across compaction.

**Two implementations of that filter, kept in lockstep.** The write-side
snapshot — `load_active_session_messages_up_to(session, N)`, plain SQL
`superseded_by IS NULL AND ordinal <= N` — is *time-sensitive*: it returns what
is active *right now*. The read-side reconstruction (trace hydration over
`load_session_messages_with_supersede`, which loads every row plus its marker)
applies the `superseded_by > N` form above. They agree only at the instant
before the referenced rows are superseded — which holds because a reference is
captured at call time (rows still active) and the at-most-one-compaction-in-
flight invariant rules out a concurrent supersede. A differential test pins the
equivalence so the two filters can't drift apart silently. Three anchoring
helpers — `latest_session_ordinal`, `count_active_messages`,
`active_index_of_ordinal` — exist to anchor and validate those references; the
sync/backfill read surface (`load_active_session_messages_tail` / `_since`,
`find_message_ordinal_by_platform_msg_id`) is covered in
[sync-protocol.md](../sync-protocol.md).

**Recovery read — full load today, a deferred bound.** Another consumer of
`load_session_messages_with_supersede` is the post-compaction transcript
recovery read: the compaction summary embeds a virtual `logs/sessions/<id>.jsonl`
path, and a `Read` of it is served by `baybo_agent`'s `SessionTranscriptReader`
(`runtime/virtual_read.rs`) from this full row set — rendered to readable text,
capped at 16 MiB per read. So the load is O(all rows) on every such read: fine
for realistic sessions on a rare path, but unbounded for a pathologically
long-lived one. The deferred optimization is a `LIMIT`-pushdown variant —
`load_session_messages_with_supersede_prefix(session, max_rows)` (`… ORDER BY
ordinal LIMIT ?`, supersede-inclusive) plus threading the read's line window
(`offset + limit`) into the resolver. Because each rendered row is ≥ ~2 lines,
loading `end = offset + limit` rows always covers an `end`-line window, so
`limit: 1` loads ~1 row instead of all. It is intentionally **not done**: the
16 MiB render cap already prevents catastrophic allocation, and even with the
`LIMIT` a high `offset` still loads O(offset) rows (line offsets don't map to row
offsets) and sequential full pagination stays O(N²) without a per-session render
cache — disproportionate for a rare path until real sessions grow large. A true
sqlite row-cursor (`rows.next()` is lazy, stop early) is avoided because the
`dyn SessionStore` boundary would force `Pin<Box<dyn Stream>>` and the single
shared `Arc<sqlite::Connection>` would be held open across the render.

### Session planning checklist: the `session_tasks` table

The `Task*` planning checklist (see [`task.md`](task.md)) lives in its own
`session_tasks` table — one row per task, keyed `(session_id, task_id)` with
`REFERENCES sessions(id) ON DELETE CASCADE`, served by `SqliteTaskStore` over the
`TaskStore` trait. It is **not** a `SessionState` field: a live checklist is
mutated on every `TaskUpdate`, concurrently with the full-blob writers on the
`sessions` row (`touch`, the actor's `save`), so a blob `Vec` would lose updates
to that clobber race. A dedicated table gives the same anti-clobber property the
`hidden` / `last_llm` flat columns buy, but at row granularity — each
`TaskUpdate` is `UPDATE … WHERE session_id=? AND task_id=?`, touching only its
own row.

Task rows are removed by `TaskUpdate(status: "deleted")` — a plain single-row
`DELETE` of one task — or reaped by the `sessions` cascade; never a session or
transcript row, so the never-delete-session-data rule is never in tension here. `TaskStore::list` skips
a row whose stored `status` is unrecognized (written by a future variant) rather
than failing the whole call, mirroring the session-blob skip discipline. Task
rows themselves carry no unread/acknowledged state.

### Single backend: sqlite

All store implementations use sqlite (async-native, SQLite-compatible). There is no rusqlite or separate in-memory backend. `Store::open(path)` opens (or creates) a file-backed sqlite database (creating parent directories if missing); `SqlitePool::open_in_memory()` is still available for tests. `SqlitePool` wraps a shared `sqlite::Connection` behind `Arc` for cheap cloning across async tasks.

The database file path is not a user-facing config knob. Bootstrap composes it from the project root via `boot::storage_db_path()` — storage always lives at `<workspace.path>/state/storage.db`. Operators pick the project root; the storage layout underneath it is fixed by convention.

### One struct per trait

Each domain has an independent Store implementation (`SqliteSessionStore`, `SqliteCostStore`, etc.). All share one `SqlitePool` while preserving domain isolation. No giant struct implementing every interface.

### Store solves initialization, not abstraction

At startup: `Store::open(path)` creates a `SqlitePool` and wires all store implementations. The `agent` layer injects individual stores into corresponding managers.

### Each Store manages its own queries

Table schemas are created centrally in `SqlitePool::init_db()`, but each Store struct owns its own query logic. No cross-domain aggregate tables.

### JSON field strategy

Fields difficult to fully structure (`SessionState.extra`, `Job.input` / `Job.final_result`) are stored as JSON. The trace stack stores the entire entity as a JSON `data` blob; queryable fields (`job_id`, `step_id`, `started_at`, `ended_at`) surface as `GENERATED ALWAYS AS (json_extract(...)) VIRTUAL` columns SQLite keeps in lockstep with `data` automatically — no two-side write contract for the storage layer to enforce. The security requirement still applies: values must already be sanitized before persistence.

### Transaction boundaries

Use transactions wherever a multi-statement write must be atomic — most importantly `SessionStore::delete`, which cascades the session's `session_messages` rows and removes the parent row in one `BEGIN IMMEDIATE` transaction (a non-transactional implementation could strand a transcript under a concurrent write).

Session rows and transcripts are user-facing core data: runtime/background
cleanup must not call the delete path. It exists for explicit destructive flows
initiated by the user.

### Hard delete everywhere but `cron_jobs` and `deck_cards`

Deletion is a plain `DELETE FROM` in every table but two: no tombstone column, no revival semantics, once a row is gone it is gone. The one cadence-driven retention sweep in `baybo-janitor` is `channel_pairings` (expired/abandoned auth-flow rows), which issues the same `DELETE FROM` against rows past their retention horizon. Blobs are **not** swept on a TTL; there is no `BlobStore::purge_older_than` API, so a blob row lives until an explicit `BlobStore::delete` removes it (which unlinks the content-addressed payload once no live row still references it).

**`cron_jobs` is the first exception: it soft-deletes.** The table carries a `deleted_at INTEGER` tombstone (Unix µs; NULL = live), `CronStore::delete` stamps it, `CronStore::restore` clears it, and no code path anywhere issues a `DELETE FROM cron_jobs`.

The reason is that a cron job's output outlives the job. Each fire leaves a `cron_executions` row, a session with a full transcript, and — for a one-shot — a notification appended into the conversation that scheduled it. Those are permanent (session rows and transcripts are core data that is never deleted), and the job row is the only thing that ties them to where they came from. Drop it and every one of them is stranded: an execution row points at a `job_id` that resolves to nothing, and a conversation that opened by itself can no longer say which scheduled task opened it. So the row stays, and `CronStore::get` keeps resolving it by id after deletion; only the *listings* stop returning it. `deleted_at` is orthogonal to the job's `status` — a deleted one-shot that already fired stays `executed` — so a restore can put the job back in exactly the state it was taken away in.

What makes the tombstone safe is that the filter lives in SQL, not in Rust: `list_due`, `list_enabled`, `list_by_user` and `list_all` all carry `WHERE … deleted_at IS NULL`, so a deleted job cannot reach the scheduler's tick loop or a user's list. `list_deleted` is the sole query that inverts it, for the recycle-bin view. Two partial indexes back this: `idx_cron_jobs_live_due` on `(status, next_trigger_at) WHERE deleted_at IS NULL` for the tick query, and `idx_cron_jobs_deleted` on `(deleted_at) WHERE deleted_at IS NOT NULL` for the bin. The full delete/restore contract — including why a restored job's `next_trigger_at` is recomputed from now — is in [`cron.md`](cron.md).

**`deck_cards` is the second, with one difference: its recycle bin can be emptied.** The mechanics mirror `cron_jobs` — a `deleted_at INTEGER` tombstone (Unix µs; NULL = live) that `DeckCardStore::set_deleted` stamps and clears, the filter in SQL (`list_live`, `count_live`, `set_layout` and the `record_snapshot` seq bump all carry `WHERE … deleted_at IS NULL`, backed by the partial index `idx_deck_cards_live` on `(position) WHERE deleted_at IS NULL`), `get` still resolving a deleted row by id, and `list_deleted` inverting the filter for the recycle-bin view. What differs is what the tombstone protects: nothing outlives a card — its `deck_snapshots` are ephemeral render state, pruned to a small latest-N by a plain `DELETE` on every insert (the push counter survives pruning because `last_seq` lives on the card row) — so a hard delete strands nothing. Hence `DeckCardStore::purge`: user-triggered from the bin (`DeckManager` refuses to purge a card that is not already deleted), never a background sweep, it removes the row and its snapshots in one transaction and the manager deletes the bundle directory.

## Constraints

- Normal dependencies are just `baybo-store` (trait contracts + row/DTO types) and `baybo-model` (domain types) — no domain crate; reverse edges from any domain crate back to `baybo-storage` do not exist. `baybo-job` / `baybo-trace` are `dev-dependencies` only (round-trip tests)
- Exposes trait objects externally, not concrete backend types
- Assumes upper layers have already sanitized data before persistence

## Collaboration

| Module                                   | Role                                                                                      |
| ---------------------------------------- | ----------------------------------------------------------------------------------------- |
| `storage` (self)                         | Provides sqlite implementations for every Store trait from `baybo-store`; owns queries and schema initialization |
| `store`                                  | Owns every `*Store` trait contract + its row/DTO types; `storage` implements them and depends only on this crate (+ `model`) |
| `model` / `trace` / `job`                | Provide domain types the sqlite impls round-trip (`trace` / `job` are `dev-dependencies` only, for the round-trip tests) |
| `context`                                | Owns `ContextManager`; pure in-memory                                                     |
| `session`                                | Owns the `SessionManager` facade and calls `SessionStore` / `SessionSummaryStore` (whose traits live in `baybo-store`); `storage` does **not** depend on `session` |
| `agent`                                  | Injects stores into managers (JobLifecycle, etc.); re-exports SessionManager |
