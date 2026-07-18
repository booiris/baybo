mod agent_profile;
mod blob;
mod channel_bot;
mod channel_pairing;
mod channel_session;
mod cost;
mod cron;
mod device;
mod job;
mod search;
mod secret;
mod session;
mod session_folder;
mod session_summary;
mod skill_risk;
mod task;
mod time;
mod trace;

pub use agent_profile::SqliteAgentProfileStore;
pub use blob::SqliteBlobStore;
pub use channel_bot::SqliteChannelBotStore;
pub use channel_pairing::SqliteChannelPairingStore;
pub use channel_session::SqliteChannelSessionStore;
pub use cost::SqliteCostStore;
pub use cron::SqliteCronStore;
pub use device::SqliteDeviceStore;
pub use job::SqliteJobStore;
pub use search::SqliteMessageSearchStore;
pub use secret::SqliteSecretStore;
pub use session::SqliteSessionStore;
pub use session_folder::SqliteSessionFolderStore;
pub use session_summary::SqliteSessionSummaryStore;
pub use skill_risk::SqliteSkillRiskStore;
pub use task::SqliteTaskStore;
pub use trace::SqliteTraceStore;

use std::sync::atomic::{AtomicU64, Ordering};

use baybo_store::StorageError;
use deadpool_sqlite::{Config, Runtime};

/// Connections kept open by the pool. Readers never block each other under
/// WAL; writers serialise on the write lock regardless of how many handles
/// exist, so a bigger pool buys read parallelism and nothing else.
const POOL_SIZE: usize = 8;

/// Size the WAL is truncated back to whenever a reset finds it larger.
///
/// Sqlite never shrinks a WAL on its own: a checkpoint *resets* it and the next
/// writer overwrites from frame 1, leaving the file at its all-time high-water
/// mark forever. Sqlite's default of no limit therefore prices the file at the
/// largest burst it has ever seen, however brief and however long ago.
///
/// This bounds only what a reset truncates *back to*, never how far a
/// transaction may grow the file — an oversized write still succeeds and
/// collapses at the next reset. Sitting an order of magnitude above the ~4 MiB
/// that the default 1000-page `wal_autocheckpoint` resets at is deliberate: the
/// limit is here to collapse a pathological high-water mark, not to police
/// ordinary traffic, which never comes near it and so never pays
/// truncate-and-regrow churn.
const WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// A second writer waits for the current one rather than failing. Concurrent
/// writers are routine here — the agent loop and the trace sink write while the
/// gateway serves reads — and sqlite's default of 0 would turn that normal
/// overlap into spurious `SQLITE_BUSY`. Contention that outlives this timeout is
/// a different animal (a *cross-process* writer, i.e. the CLI holding the file
/// against a running gateway) and is handled by [`crate::retry`].
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Pool of sqlite connections.
///
/// Cheap to clone (the inner `deadpool` pool is an `Arc`) and shared by every
/// store. Callers reach the database only through [`SqlitePool::interact`],
/// which checks a connection out *exclusively* for the whole closure.
///
/// That exclusivity is a memory-safety contract, not a throughput knob. A
/// sqlite connection owns an unsynchronised private heap — its lookaside
/// allocator — and the C API's own accessors mutate it: `sqlite3_value_text()`
/// on a TEXT column allocates in order to NUL-terminate. The decode is
/// therefore as much a critical section as the query, and two threads inside
/// one handle corrupt the free list; the process dies later, in an unrelated
/// allocation. `rusqlite::Connection` is `Send` but *not* `Sync`, so the
/// compiler — not a convention — is what keeps them out.
#[derive(Clone)]
pub struct SqlitePool {
    pool: deadpool_sqlite::Pool,
}

impl SqlitePool {
    /// Open (or create) a local sqlite database at the given path.
    pub async fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", parent.display()))?;
        }
        Self::build(Config::new(path), path.display().to_string()).await
    }

    /// Open a private in-memory database (tests).
    ///
    /// A bare `:memory:` would hand every pooled connection its own empty
    /// database. The shared-cache URI makes the pool's connections address one
    /// database, and the unique name keeps concurrent tests isolated from each
    /// other. Such a database lives only while a connection to it is open, so
    /// the pool must never reap idle connections — deadpool doesn't by default.
    pub async fn open_in_memory() -> anyhow::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let uri = format!("file:baybo-test-{id}?mode=memory&cache=shared");
        Self::build(Config::new(&uri), uri.clone()).await
    }

    async fn build(cfg: Config, what: String) -> anyhow::Result<Self> {
        let pool = cfg
            .builder(Runtime::Tokio1)
            .map_err(|e| anyhow::anyhow!("failed to configure sqlite pool for {what}: {e}"))?
            .max_size(POOL_SIZE)
            // Per-connection state, so it belongs on the hook that fires for
            // every connection the pool ever creates — including ones opened
            // lazily under load, or replaced after a recycle. `journal_mode` is
            // persisted in the file header and only needs saying once, but
            // `synchronous`, `busy_timeout` and `journal_size_limit` are
            // per-handle and would otherwise silently revert to sqlite's
            // defaults on a fresh handle. The limit has to reach every
            // connection because any of them may be the one that resets the WAL.
            .post_create(deadpool_sqlite::Hook::async_fn(|conn, _| {
                Box::pin(async move {
                    conn.interact(|conn| {
                        conn.busy_timeout(BUSY_TIMEOUT)?;
                        conn.pragma_update(None, "journal_mode", "WAL")?;
                        conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
                        conn.pragma_update(None, "synchronous", "NORMAL")
                    })
                    .await
                    .map_err(|e| deadpool_sqlite::HookError::message(e.to_string()))?
                    .map_err(deadpool_sqlite::HookError::Backend)
                })
            }))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build sqlite pool for {what}: {e}"))?;
        let pool = Self { pool };
        pool.interact("sqlite.init_db", init_db)
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize schema for {what}: {e}"))?;
        pool.warm(&what).await?;
        Ok(pool)
    }

    /// Open every connection now, rather than letting the pool do it lazily on
    /// first contention.
    ///
    /// `deadpool` opens a connection on a blocking thread, and a *cancelled*
    /// creation is an unconditional panic inside `deadpool-sync` (unlike a
    /// cancelled query, which it reports as an error). The tokio runtime cancels
    /// queued blocking tasks when it shuts down — so a pool still opening
    /// connections during shutdown panics a worker. Shutdown is exactly when
    /// that is likely: every actor flushes its state at once, which for a
    /// half-warm pool is the first real contention it has ever seen.
    ///
    /// Holding all the connections at once is what forces distinct ones; getting
    /// them one at a time would hand back the same connection every time.
    async fn warm(&self, what: &str) -> anyhow::Result<()> {
        let mut conns = Vec::with_capacity(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            conns.push(
                self.pool
                    .get()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to warm sqlite pool for {what}: {e}"))?,
            );
        }
        Ok(())
    }

    /// Run `f` against a connection held exclusively for the whole closure.
    ///
    /// `f` runs on a blocking thread (rusqlite is synchronous), so it must own
    /// its inputs — bind every parameter as an owned value. `op` names the
    /// call-site and prefixes any error.
    pub(crate) async fn interact<F, T>(&self, op: &'static str, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("{op}: pool checkout: {e}")))?;
        conn.interact(f)
            .await
            // The closure panicked, or a previous one did and poisoned the
            // connection. Either way it never produced a result.
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("{op}: {e}")))?
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("{op}: {e}")))
    }
}

/// `?,?,...,?` for an `IN (...)` list of `n` bound values. Callers
/// chunk large lists (see `IN_CHUNK` sites) to stay under sqlite's
/// bound-variable limit.
pub(crate) fn in_placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

/// Create all required tables if they do not already exist.
///
/// Timestamp columns (`created_at`, `started_at` on `jobs`, etc.) are Unix
/// microseconds — round-trip via `sqlite::time::{to_us, from_us}`. µs is
/// finer than the millisecond granularity of typical web tooling so sub-ms
/// ordering survives (useful for fast local tool spans), and
/// `chrono::timestamp_micros` is infallible. API surfaces (HTTP /
/// OpenAPI / web) re-encode as RFC3339 and don't expose raw µs.
///
/// **Exception — trace tables (`steps`, `spans`).** The `started_at`
/// / `ended_at` columns are TEXT generated columns extracted from
/// the JSON `data` blob via `json_extract`. `baybo-trace` serialises
/// `chrono::DateTime<Utc>` as RFC3339 strings, so these columns
/// hold RFC3339 — sortable lexicographically because the leading
/// `YYYY-MM-DDTHH:MM:SS` prefix is fixed-width and any
/// fractional-second suffix shares a common prefix length within
/// a single insertion path. They don't follow the µs invariant
/// the rest of the schema uses; querying these columns means
/// string comparison, not integer comparison.
fn init_db(conn: &mut rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    id                    TEXT PRIMARY KEY,
                    root_session_id       TEXT NOT NULL,
                    trigger_kind          TEXT NOT NULL,
                    parent_session_id     TEXT,
                    parent_job_id         TEXT,
                    -- `ToolCall(spawn_subagent)` span on the parent
                    -- that birthed this session, recorded so trace
                    -- viewers can hop from the parent's span to the
                    -- child's session and so sibling subagents from
                    -- one parent job stay distinguishable. NULL for
                    -- non-subagent (root) sessions and for sessions
                    -- migrated from before this column existed.
                    parent_span_id        TEXT,
                    lineage_kind          TEXT,
                    created_at            INTEGER NOT NULL,
                    last_active           INTEGER NOT NULL,
                    -- User-facing chat-list hide flag, set by
                    -- DELETE /v1/chat/sessions/:id. Filtered at the
                    -- chat API layer only; SessionStore::list_all
                    -- does NOT exclude hidden rows, so admin / trace
                    -- surfaces still see them.
                    hidden                INTEGER NOT NULL DEFAULT 0,
                    -- Per-session LLM pin (chat model switch). Flat column,
                    -- NULL ⇒ follow `default-llm`. Like `hidden`, it is owned
                    -- by a targeted UPDATE (`set_last_llm`) and the DO UPDATE
                    -- in `save` omits it, so a concurrent `touch` (load + full
                    -- save) can't clobber a just-set pin; `get` patches
                    -- `Session.state.last_llm` from this column on read.
                    last_llm              TEXT,
                    -- User-facing chat-list pin flag, set by
                    -- PUT /v1/chat/sessions/:id/pin. Like `hidden` /
                    -- `last_llm` it is a flat column owned by a targeted
                    -- UPDATE (`set_pinned`) and omitted from the DO UPDATE
                    -- in `save`, so a concurrent `touch` (load + full save)
                    -- can't clobber a just-set pin; `get` patches
                    -- `Session.pinned` from this column on read.
                    pinned                INTEGER NOT NULL DEFAULT 0,
                    -- User-facing chat-list archive flag, set by
                    -- PUT /v1/chat/sessions/:id/archive. Presentation
                    -- only — the chat list endpoint returns it on every
                    -- row and never filters on it; clients group
                    -- archived rows themselves. Like `pinned` it is a
                    -- flat column owned by a targeted UPDATE
                    -- (`set_archived`) and omitted from the DO UPDATE
                    -- in `save`; `get` patches `Session.archived` from
                    -- this column on read.
                    archived              INTEGER NOT NULL DEFAULT 0,
                    -- User-facing chat-list folder assignment, set by
                    -- PUT /v1/chat/sessions/:id/folder. NULL ⇒ uncategorized.
                    -- Like `pinned` / `last_llm` it is a flat column owned by
                    -- a targeted UPDATE (`set_folder`) and omitted from the DO
                    -- UPDATE in `save`, so a concurrent `touch` can't clobber a
                    -- just-set assignment; `get` patches `Session.folder_id`
                    -- from this column on read. No FK to session_folders —
                    -- SQLite FKs are off (see set_wal_mode), so folder delete
                    -- nulls this column manually.
                    folder_id             TEXT,
                    -- Per-session read cursor for the chat-list unread badge:
                    -- the highest `session_messages.ordinal` a viewer has read.
                    -- Set (max-wins) by PUT /v1/chat/sessions/:id/read; the
                    -- list endpoint derives `unread_count` = visible replies
                    -- with ordinal > read_cursor. Like the other chat-list flat
                    -- columns it is owned by a targeted UPDATE (`set_read_cursor`)
                    -- and omitted from the DO UPDATE in `save`, so a concurrent
                    -- `touch` can't clobber it. NULL ⇒ nothing read yet.
                    read_cursor           INTEGER,
                    -- Auto-generated conversation title; owned by set_title.
                    title                 TEXT,
                    data                  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_sessions_root
                    ON sessions(root_session_id);
                -- User-created folders for organising the chat-session list.
                -- Two-level tree via self-referential `parent_id` (NULL =
                -- top-level; the depth cap of 2 is enforced in the session
                -- manager, not here). This is the PARENT entity —
                -- sessions.folder_id points into it — so it is NOT a
                -- per-session CASCADE child; deleting a folder dissolves the
                -- grouping in code (nulls child sessions, promotes
                -- sub-folders) and never removes session rows.
                CREATE TABLE IF NOT EXISTS session_folders (
                    id          TEXT PRIMARY KEY,
                    parent_id   TEXT,
                    name        TEXT NOT NULL,
                    position    INTEGER NOT NULL,
                    created_at  INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_session_folders_parent
                    ON session_folders(parent_id);
                CREATE INDEX IF NOT EXISTS idx_sessions_parent
                    ON sessions(parent_session_id, lineage_kind)
                    WHERE lineage_kind IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_sessions_parent_span
                    ON sessions(parent_span_id)
                    WHERE parent_span_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_sessions_last_active
                    ON sessions(last_active DESC);
                -- Append-only per-message log. One row per appended
                -- ChatMessage; `/compact` does not delete or rewrite
                -- prior rows — it inserts the summary message(s) at
                -- the next ordinal and bulk-marks earlier active rows
                -- with `superseded_by = first_summary_ordinal`. The
                -- active LLM context is the rows where
                -- `superseded_by IS NULL` ordered by `ordinal`; the
                -- full historical transcript ignores the column.
                CREATE TABLE IF NOT EXISTS session_messages (
                    session_id    TEXT NOT NULL,
                    ordinal       INTEGER NOT NULL,
                    role          TEXT NOT NULL,
                    content       TEXT NOT NULL,
                    created_at    INTEGER NOT NULL,
                    superseded_by INTEGER,
                    -- Provenance of the row (`baybo_model::MessageSource`):
                    -- 'user' (a genuine channel input), 'cron' (a cron fire's
                    -- framed prompt), or 'agent' (everything else the agent
                    -- injects/produces). The agent appends several
                    -- `role = 'user'` rows (skill reminders, the cron fire,
                    -- subagent tasks); this column tells the genuine prompt and
                    -- the cron fire apart from them without guessing by content.
                    source        TEXT NOT NULL DEFAULT 'agent',
                    -- Client idempotency key for genuine channel input. Used by
                    -- reconnect/history replay to reconcile optimistic user
                    -- bubbles; empty for rows without a client-supplied id.
                    platform_msg_id TEXT NOT NULL DEFAULT '',
                    -- Durable idempotency key for a source event that may be
                    -- replayed after a crash (for example a cron execution or
                    -- a background-notification batch operation). NULL for
                    -- ordinary transcript rows.
                    source_event_id TEXT,
                    PRIMARY KEY (session_id, ordinal)
                );
                CREATE INDEX IF NOT EXISTS idx_session_messages_active
                    ON session_messages(session_id, ordinal)
                    WHERE superseded_by IS NULL;

                -- Identity of the pipeline that produced `message_fts`, so a
                -- changed segmenter rebuilds instead of leaving half the index
                -- speaking a different alphabet than the queries. `message_fts`
                -- itself is NOT created here: it is dropped and recreated by
                -- `search::rebuild_if_stale` off this fingerprint, because
                -- `CREATE ... IF NOT EXISTS` cannot migrate a column onto a table
                -- that already exists. See `docs/search.md`.
                CREATE TABLE IF NOT EXISTS search_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                -- Out-of-band events shown in the chat transcript but NOT part
                -- of the LLM conversation: a user's control-command echo
                -- (`/stop`, `/compact`) and the resulting notices (plus any other
                -- out-of-band notice). Kept out of `session_messages` on purpose
                -- so that table stays exactly the LLM context — no filtering,
                -- intact ordinal/marker invariants, accurate trace inputs. The
                -- chat view interleaves these by `after_ordinal` (the
                -- `session_messages.ordinal` the event follows, or -1 if none
                -- yet) so they land in the right page on scroll-up too. `kind` is
                -- one of 'command' / 'notice_info' / 'notice_warn' /
                -- 'notice_error' (`baybo_model::ControlEventKind`); `seq` is a
                -- per-session monotonic id (stable key + same-anchor tiebreak);
                -- `created_at` is the event's own time, shown in the UI.
                CREATE TABLE IF NOT EXISTS session_control_events (
                    session_id    TEXT    NOT NULL,
                    seq           INTEGER NOT NULL,
                    after_ordinal INTEGER NOT NULL,
                    kind          TEXT    NOT NULL,
                    text          TEXT    NOT NULL,
                    created_at    INTEGER NOT NULL,
                    PRIMARY KEY (session_id, seq)
                );

                -- Per-session summary metadata. Content lives on disk at
                -- `<workspace>/state/sessions/<session_id>/summary.md`; this
                -- row is the durable, queryable index. ON DELETE CASCADE
                -- with sessions so removing a parent removes its summary
                -- row automatically. The on-disk file is reaped separately
                -- on startup (orphan FS sweep).
                --
                -- See `docs/background-compression.md`.
                CREATE TABLE IF NOT EXISTS session_summaries (
                    session_id  TEXT    PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                    -- session_messages.ordinal of the most-recent message
                    -- included in the last successful summary pass.
                    cursor      INTEGER NOT NULL,
                    pass_count  INTEGER NOT NULL DEFAULT 0,
                    -- Unix µs (matches the rest of the µs schema).
                    updated_at  INTEGER NOT NULL,
                    -- Cumulative micro-USD spent on this session's summary
                    -- passes. INTEGER, never REAL — same `feedback_money_no_float`
                    -- invariant as `cost_records.cost_usd`.
                    cost_micros INTEGER NOT NULL DEFAULT 0,
                    model_id    TEXT    NOT NULL,
                    span_id     TEXT    NOT NULL,
                    -- Telemetry only — does NOT gate triggers. A persistent
                    -- failure burns one LLM call per trigger event until the
                    -- underlying issue resolves; that's an explicit design
                    -- choice (no backoff complexity).
                    error_count INTEGER NOT NULL DEFAULT 0,
                    -- LEGACY (inert): the DB-flag at-most-one-in-flight
                    -- mechanism for background compression was removed when
                    -- the pass moved to an in-actor detached step gated by an
                    -- in-memory JoinHandle. The columns stay in the schema so
                    -- old DBs need no migration; nothing reads or writes them.
                    in_flight   INTEGER NOT NULL DEFAULT 0,
                    in_flight_owner TEXT
                );

                -- The session planning checklist (Task*). One row
                -- per task; each TaskUpdate is a per-row UPDATE so it never
                -- clobbers, and is never clobbered by, the full-blob writers
                -- on the `sessions` row. CASCADE reaps tasks on user-triggered
                -- session delete; the runtime never sweeps them.
                CREATE TABLE IF NOT EXISTS session_tasks (
                    session_id  TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    task_id     TEXT    NOT NULL,
                    -- Brief title.
                    subject     TEXT    NOT NULL,
                    -- Task body (what needs to be done).
                    description TEXT    NOT NULL,
                    -- TaskStatus::as_str(); an unrecognized value (future
                    -- variant) is skipped on read, never 500s the list.
                    status      TEXT    NOT NULL,
                    -- JSON array of TaskId strings (advisory ordering).
                    depends_on  TEXT    NOT NULL DEFAULT '[]',
                    -- Unix µs (matches the rest of the µs schema).
                    created_at  INTEGER NOT NULL,
                    updated_at  INTEGER NOT NULL,
                    PRIMARY KEY (session_id, task_id)
                );
                CREATE INDEX IF NOT EXISTS idx_session_tasks_session
                    ON session_tasks(session_id);

                CREATE TABLE IF NOT EXISTS secrets (
                    name            TEXT PRIMARY KEY,
                    encrypted_value BLOB NOT NULL
                );

                CREATE TABLE IF NOT EXISTS cost_records (
                    id                              INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id                         TEXT    NOT NULL,
                    session_id                      TEXT    NOT NULL,
                    job_id                          TEXT    NOT NULL,
                    span_id                         TEXT    NOT NULL,
                    -- Call purpose (CallReason). Nullable: rows written before
                    -- this column read NULL and map to the default reason.
                    reason                          TEXT,
                    model                           TEXT    NOT NULL,
                    input_tokens                    INTEGER NOT NULL,
                    output_tokens                   INTEGER NOT NULL,
                    cached_input_tokens             INTEGER NOT NULL DEFAULT 0,
                    cache_creation_input_tokens     INTEGER NOT NULL DEFAULT 0,
                    -- Spend in micro-USD (USD × 10^6). INTEGER, never REAL —
                    -- floats accumulate rounding error across SUM() and
                    -- quota comparisons.
                    cost_usd                        INTEGER NOT NULL,
                    timestamp                       INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_cost_user_id ON cost_records(user_id);
                CREATE INDEX IF NOT EXISTS idx_cost_user_ts ON cost_records(user_id, timestamp);
                CREATE INDEX IF NOT EXISTS idx_cost_timestamp ON cost_records(timestamp);
                CREATE INDEX IF NOT EXISTS idx_cost_session ON cost_records(session_id);
                CREATE INDEX IF NOT EXISTS idx_cost_job ON cost_records(job_id);

                CREATE TABLE IF NOT EXISTS jobs (
                    id                       TEXT PRIMARY KEY,
                    session_id               TEXT NOT NULL,
                    parent_job_id            TEXT,
                    kind                     TEXT NOT NULL,
                    status_kind              TEXT NOT NULL,
                    created_at               INTEGER NOT NULL,
                    started_at               INTEGER,
                    ended_at                 INTEGER,
                    data                     TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_jobs_session
                    ON jobs(session_id, created_at);
                CREATE INDEX IF NOT EXISTS idx_jobs_status
                    ON jobs(status_kind);
                CREATE INDEX IF NOT EXISTS idx_jobs_created
                    ON jobs(created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_jobs_parent
                    ON jobs(parent_job_id);

                CREATE TABLE IF NOT EXISTS job_transitions (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id     TEXT NOT NULL,
                    data       TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_job_transitions_job_id ON job_transitions(job_id);

                -- Trace tables: a single canonical JSON `data` blob per
                -- row plus VIRTUAL generated columns extracted by
                -- `json_extract` for the indexed lookups. SQLite keeps
                -- the virtual columns in lockstep with `data`, so there
                -- is no two-side write contract for the storage layer
                -- to enforce — adding a new field is a serde change in
                -- `baybo-trace`, no schema migration.
                CREATE TABLE IF NOT EXISTS steps (
                    id         TEXT PRIMARY KEY,
                    data       TEXT NOT NULL,
                    job_id     TEXT GENERATED ALWAYS AS (json_extract(data, '$.job_id')) VIRTUAL,
                    started_at TEXT GENERATED ALWAYS AS (json_extract(data, '$.started_at')) VIRTUAL,
                    ended_at   TEXT GENERATED ALWAYS AS (json_extract(data, '$.ended_at')) VIRTUAL
                );
                CREATE INDEX IF NOT EXISTS idx_steps_job
                    ON steps(job_id, started_at);
                -- Serves the boot recovery sweep: only genuinely open
                -- rows are read instead of json_extract-scanning every
                -- step/span blob.
                CREATE INDEX IF NOT EXISTS idx_steps_open
                    ON steps(id) WHERE ended_at IS NULL;

                CREATE TABLE IF NOT EXISTS spans (
                    id         TEXT PRIMARY KEY,
                    data       TEXT NOT NULL,
                    step_id    TEXT GENERATED ALWAYS AS (json_extract(data, '$.step_id')) VIRTUAL,
                    started_at TEXT GENERATED ALWAYS AS (json_extract(data, '$.started_at')) VIRTUAL,
                    ended_at   TEXT GENERATED ALWAYS AS (json_extract(data, '$.ended_at')) VIRTUAL
                );
                CREATE INDEX IF NOT EXISTS idx_spans_step
                    ON spans(step_id, started_at);
                CREATE INDEX IF NOT EXISTS idx_spans_open_step
                    ON spans(step_id) WHERE ended_at IS NULL;

                CREATE TABLE IF NOT EXISTS span_events (
                    span_id         TEXT    NOT NULL,
                    seq             INTEGER NOT NULL,
                    data            TEXT    NOT NULL,
                    -- Outer SpanEventKind tag ('sanitize_hit' | 'approval'
                    -- | 'tool_event'); extracted from the JSON blob so
                    -- the writer never has to populate it explicitly.
                    kind            TEXT
                        GENERATED ALWAYS AS (json_extract(data, '$.kind.kind')) VIRTUAL,
                    -- Inner ToolEventPayload tag ('phase' | 'http_fetch'
                    -- | 'llm_call'); NULL for non-tool_event rows. The
                    -- nested path means SQLite returns NULL automatically
                    -- when the outer kind is not `tool_event`.
                    tool_event_kind TEXT
                        GENERATED ALWAYS AS (json_extract(data, '$.kind.payload.type')) VIRTUAL,
                    PRIMARY KEY (span_id, seq)
                );
                CREATE INDEX IF NOT EXISTS idx_span_events_kind
                    ON span_events(kind, tool_event_kind);

                CREATE TABLE IF NOT EXISTS cron_jobs (
                    id              TEXT    PRIMARY KEY,
                    user_id         TEXT    NOT NULL,
                    status          TEXT    NOT NULL,
                    -- Unix µs; 0 means 'no scheduled fire'
                    -- (replaces the empty-string sentinel from the prior
                    -- TEXT/RFC3339 schema).
                    next_trigger_at INTEGER NOT NULL DEFAULT 0,
                    -- Recycle bin (Unix µs; NULL = live). Orthogonal to
                    -- `status`. Every listing filters on `deleted_at IS NULL`
                    -- in SQL, which is what keeps a deleted job out of the
                    -- tick loop; the full value also rides in `data`.
                    deleted_at      INTEGER,
                    -- Whether the job's cron GROUP is pinned in the chat list
                    -- (docs/cron-groups.md). A flat column, never the `data`
                    -- blob (like `deleted_at`): every blob write reconstructs the
                    -- row from a snapshot the caller holds and `record_fire`
                    -- re-serializes it on every fire, so a pin in the blob would
                    -- be reverted by the next tick. Written only by `set_pinned`.
                    pinned          INTEGER NOT NULL DEFAULT 0,
                    data            TEXT    NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_cron_jobs_user_id ON cron_jobs(user_id);
                CREATE INDEX IF NOT EXISTS idx_cron_jobs_due ON cron_jobs(status, next_trigger_at);

                CREATE TABLE IF NOT EXISTS cron_executions (
                    id                  TEXT    PRIMARY KEY,
                    job_id              TEXT    NOT NULL,
                    user_id             TEXT    NOT NULL,
                    scheduled_fire_time INTEGER NOT NULL DEFAULT 0,
                    triggered_at        INTEGER NOT NULL,
                    status              TEXT    NOT NULL DEFAULT 'pending',
                    -- Delivery ledger for a one-shot fire's result (Unix µs;
                    -- NULL = not yet). `completed_at` is set when the fire's
                    -- turn ends, `notified_at` when its result reached the
                    -- origin conversation (or was terminally dropped) — the
                    -- pair drives the boot re-drive scan. The full ledger
                    -- (outcome, reply ordinal, fire session) rides in `data`;
                    -- these two are columns so the scan is a query, not a
                    -- full-table deserialize.
                    completed_at        INTEGER,
                    notified_at         INTEGER,
                    data                TEXT    NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_cron_executions_job_id ON cron_executions(job_id);
                CREATE INDEX IF NOT EXISTS idx_cron_executions_user_id ON cron_executions(user_id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_executions_dedup ON cron_executions(job_id, scheduled_fire_time);
                CREATE INDEX IF NOT EXISTS idx_cron_executions_status ON cron_executions(status);
                CREATE INDEX IF NOT EXISTS idx_cron_executions_triggered_at ON cron_executions(triggered_at);

                CREATE TABLE IF NOT EXISTS skill_risk_assessments (
                    skill_name   TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    level        TEXT NOT NULL,
                    rationale    TEXT NOT NULL,
                    model        TEXT NOT NULL,
                    assessed_at  INTEGER NOT NULL,
                    PRIMARY KEY (skill_name, content_hash)
                );

                CREATE TABLE IF NOT EXISTS skill_risk_assessment_jobs (
                    skill_name   TEXT    NOT NULL,
                    content_hash TEXT    NOT NULL,
                    source_path  TEXT    NOT NULL,
                    status       TEXT    NOT NULL,
                    attempts     INTEGER NOT NULL DEFAULT 0,
                    last_error   TEXT,
                    created_at   INTEGER NOT NULL,
                    updated_at   INTEGER NOT NULL,
                    PRIMARY KEY (skill_name, content_hash)
                );
                CREATE INDEX IF NOT EXISTS idx_skill_risk_jobs_status
                    ON skill_risk_assessment_jobs(status);

                CREATE TABLE IF NOT EXISTS channel_sessions (
                    channel_type TEXT    NOT NULL,
                    user_id      TEXT    NOT NULL,
                    session_id   TEXT    NOT NULL,
                    created_at   INTEGER NOT NULL,
                    PRIMARY KEY (channel_type, user_id)
                );
                CREATE INDEX IF NOT EXISTS idx_channel_sessions_session
                    ON channel_sessions(session_id);

                CREATE TABLE IF NOT EXISTS channel_bots (
                    channel_type TEXT    NOT NULL,
                    bot_id       TEXT    NOT NULL,
                    created_at   INTEGER NOT NULL,
                    PRIMARY KEY (channel_type, bot_id)
                );

                CREATE TABLE IF NOT EXISTS blobs (
                    blob_id           TEXT PRIMARY KEY,
                    mime_type         TEXT NOT NULL,
                    size              INTEGER NOT NULL,
                    uploader_identity TEXT,
                    read_token        TEXT,
                    created_at        INTEGER NOT NULL,
                    last_accessed_at  INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_blobs_uploader_identity_size
                    ON blobs(uploader_identity, size)
                    WHERE uploader_identity IS NOT NULL;

                -- User-managed agent profiles (chat personas). The seeded
                -- built-in `baybo` row (builtin = 1) is read-only except its
                -- avatar: update/delete run WHERE builtin = 0 in the store
                -- impl and create never binds `builtin`, so the open()-time
                -- seed is the only writer of 1. avatar_blob_id is a soft
                -- reference into blobs (FKs are off — see set_wal_mode).
                -- Skills are not stored here — they are read live from the
                -- skill registry (see docs/modules/agent-profiles.md).
                CREATE TABLE IF NOT EXISTS agent_profiles (
                    id              TEXT PRIMARY KEY,
                    name            TEXT NOT NULL UNIQUE COLLATE NOCASE,
                    description     TEXT NOT NULL,
                    avatar_blob_id  TEXT,
                    system_prompt   TEXT,
                    framework       TEXT NOT NULL,
                    llm             TEXT,
                    builtin         INTEGER NOT NULL DEFAULT 0,
                    created_at      INTEGER NOT NULL,
                    updated_at      INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS channel_pairings (
                    channel_type TEXT    NOT NULL,
                    bot_id       TEXT    NOT NULL,
                    user_id      TEXT    NOT NULL,
                    code         TEXT    NOT NULL,
                    status       TEXT    NOT NULL,
                    created_at   INTEGER NOT NULL,
                    expires_at   INTEGER,
                    approved_at  INTEGER,
                    PRIMARY KEY (channel_type, bot_id, user_id)
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_channel_pairings_code
                    ON channel_pairings(code);

                CREATE TABLE IF NOT EXISTS devices (
                    device_id     TEXT    NOT NULL,
                    device_pubkey BLOB    NOT NULL,
                    auth_token    TEXT    NOT NULL,
                    status        TEXT    NOT NULL,
                    rendezvous_id TEXT,
                    created_at    INTEGER NOT NULL,
                    approved_at   INTEGER,
                    last_seen_at  INTEGER,
                    relay_url     TEXT    NOT NULL DEFAULT '',
                    remote_api_key  TEXT    NOT NULL DEFAULT '',
                    PRIMARY KEY (device_id)
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_auth_token
                    ON devices(auth_token);
                -- One gateway = one app: at most one Approved device at a time.
                -- A partial unique index on the (constant-valued) status column
                -- of approved rows admits exactly one such row. Re-pairing the
                -- same device refreshes its row in place; a different device
                -- supersedes the prior binding (see
                -- DeviceStore::create_replacing_approved), whose row goes Revoked
                -- and drops out of this partial index.
                CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_one_approved
                    ON devices(status) WHERE status = 'approved';",
    )
    .map_err(|e| anyhow::anyhow!("failed to initialize sqlite schema: {e}"))?;

    // Migrations for DBs created before a column was added. SQLite has no
    // `ADD COLUMN IF NOT EXISTS`, so we attempt the ALTER and swallow the
    // "duplicate column" error. Add new migrations to this list rather
    // than mutating the CREATE TABLE — fresh DBs pick the column up from
    // CREATE, existing DBs from the ALTER.
    let migrations: &[&str] = &[
        "ALTER TABLE sessions ADD COLUMN parent_span_id TEXT",
        "ALTER TABLE sessions ADD COLUMN last_llm TEXT",
        "ALTER TABLE sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE cost_records ADD COLUMN reason TEXT",
        "ALTER TABLE sessions ADD COLUMN folder_id TEXT",
        "ALTER TABLE sessions ADD COLUMN title TEXT",
        "ALTER TABLE devices ADD COLUMN relay_url TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE devices ADD COLUMN remote_api_key TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE session_messages ADD COLUMN platform_msg_id TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE sessions ADD COLUMN read_cursor INTEGER",
        "ALTER TABLE cron_executions ADD COLUMN completed_at INTEGER",
        "ALTER TABLE cron_executions ADD COLUMN notified_at INTEGER",
        "ALTER TABLE session_messages ADD COLUMN source_event_id TEXT",
        "ALTER TABLE cron_jobs ADD COLUMN deleted_at INTEGER",
        "ALTER TABLE cron_jobs ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE sessions ADD COLUMN channel TEXT",
    ];
    for stmt in migrations {
        if let Err(e) = conn.execute(stmt, []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(anyhow::anyhow!("migration `{stmt}` failed: {msg}"));
            }
        }
    }

    // Indexes on migration-added columns. Created AFTER the ALTER loop,
    // not in the schema batch above: on a legacy DB the column doesn't
    // exist until the ALTER runs, so a batch-time CREATE INDEX referencing
    // it would fail. `IF NOT EXISTS` keeps them idempotent on every
    // subsequent boot.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_sessions_folder
             ON sessions(folder_id) WHERE folder_id IS NOT NULL;
         -- Serves the boot re-drive's 'completed but not yet delivered' scan.
         CREATE INDEX IF NOT EXISTS idx_cron_executions_awaiting_delivery
             ON cron_executions(completed_at) WHERE notified_at IS NULL;
         -- Serves the tick loop's due scan, which only ever considers live rows.
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_live_due
             ON cron_jobs(status, next_trigger_at) WHERE deleted_at IS NULL;
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_deleted
             ON cron_jobs(deleted_at) WHERE deleted_at IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS idx_session_messages_source_event
             ON session_messages(session_id, source_event_id)
             WHERE source_event_id IS NOT NULL;
         -- Serves the chat-list base query (channel scope + newest-first).
         CREATE INDEX IF NOT EXISTS idx_sessions_channel_active
             ON sessions(channel, last_active DESC);",
    )
    .map_err(|e| anyhow::anyhow!("failed to create post-migration indexes: {e}"))?;

    // One-time data collapse: the retired per-surface channel tags `http`
    // (web) and `device` (mobile) were unified into a single `owner` pool
    // channel. Re-tag every pre-collapse session and cron job so it stays
    // reachable — the chat/cron scope now matches `owner`, so a lingering
    // `http`/`device` tag would scope the row out and make it invisible. The
    // channel rides in the JSON `data` blob for both tables. Idempotent:
    // after one pass the WHERE clauses match nothing. Data-preserving — only
    // the channel tag changes; transcript, summary, and all else are untouched.
    conn.execute_batch(
        "UPDATE sessions \
            SET data = json_set(json_set(data, '$.channel', 'owner'), '$.user.channel', 'owner') \
            WHERE json_extract(data, '$.channel') IN ('http', 'device') \
               OR json_extract(data, '$.user.channel') IN ('http', 'device'); \
         UPDATE cron_jobs \
            SET data = json_set(data, '$.channel', 'owner') \
            WHERE json_extract(data, '$.channel') IN ('http', 'device');",
    )
    .map_err(|e| anyhow::anyhow!("owner-channel collapse migration failed: {e}"))?;

    // Backfill the flat `channel` column from the blob for rows written
    // before the column existed. Runs AFTER the owner-collapse pass so a
    // collapsed tag lands, not the retired one. Idempotent: after one
    // pass no row has a NULL channel (new writes set it directly).
    conn.execute(
        "UPDATE sessions SET channel = json_extract(data, '$.channel') WHERE channel IS NULL",
        [],
    )
    .map_err(|e| anyhow::anyhow!("channel column backfill failed: {e}"))?;

    search::rebuild_if_stale(conn)
        .map_err(|e| anyhow::anyhow!("search index rebuild failed: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `post_create` hook is the only thing standing between the pool and
    /// sqlite's defaults, and a PRAGMA that silently fails to apply looks
    /// exactly like one that applied — the query still runs, just with the old
    /// setting. Assert the connection's actual state rather than trusting that
    /// the hook ran.
    #[tokio::test]
    async fn every_connection_gets_the_pragmas() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = SqlitePool::open(dir.path().join("t.db"))
            .await
            .expect("open");

        // Ask enough connections to cover the pool: a hook that fires only for
        // the first one would pass a single-connection check.
        for _ in 0..POOL_SIZE * 2 {
            let (journal_mode, busy_timeout, synchronous, journal_size_limit) = pool
                .interact("test.pragmas", |conn| {
                    Ok((
                        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
                        conn.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, i64>(0))?,
                        conn.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?,
                        conn.query_row("PRAGMA journal_size_limit", [], |r| r.get::<_, i64>(0))?,
                    ))
                })
                .await
                .expect("read pragmas");
            assert_eq!(journal_mode, "wal");
            assert_eq!(busy_timeout, BUSY_TIMEOUT.as_millis() as i64);
            assert_eq!(synchronous, 1, "NORMAL");
            assert_eq!(journal_size_limit, WAL_SIZE_LIMIT);
        }
    }

    /// A shared-cache in-memory database exists only while some connection to
    /// it is open. The pool must therefore keep one alive between calls —
    /// otherwise the schema would silently vanish and the next call would open
    /// a fresh, empty database.
    #[tokio::test]
    async fn in_memory_database_survives_between_checkouts() {
        let pool = SqlitePool::open_in_memory().await.expect("open");
        pool.interact("test.write", |conn| {
            conn.execute(
                "INSERT INTO secrets (name, encrypted_value) VALUES ('k', x'01')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("write");

        let n: i64 = pool
            .interact("test.read", |conn| {
                Ok(conn.query_row("SELECT count(*) FROM secrets", [], |r| r.get(0))?)
            })
            .await
            .expect("read");
        assert_eq!(n, 1, "the in-memory database outlived the first checkout");
    }

    /// The shared cache the in-memory pool relies on locks at *table* level and
    /// signals contention with `SQLITE_LOCKED`, which `busy_timeout` does not
    /// wait out. Every unit test in the crate runs on this pool, so if
    /// concurrent writers could trip that, the whole suite would flake. Prove
    /// they don't.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn in_memory_pool_tolerates_concurrent_writers() {
        let pool = SqlitePool::open_in_memory().await.expect("open");
        let mut tasks = Vec::new();
        for w in 0..8 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                for i in 0..25 {
                    let name = format!("k{w}-{i}");
                    pool.interact("test.concurrent_write", move |conn| {
                        conn.execute(
                            "INSERT INTO secrets (name, encrypted_value) VALUES (?1, x'01')",
                            rusqlite::params![name],
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("concurrent insert");
                }
            }));
        }
        for t in tasks {
            t.await.expect("writer panicked");
        }

        let n: i64 = pool
            .interact("test.count", |conn| {
                Ok(conn.query_row("SELECT count(*) FROM secrets", [], |r| r.get(0))?)
            })
            .await
            .expect("count");
        assert_eq!(n, 200, "no writer was silently dropped");
    }

    /// An open pool must already hold every connection, leaving none to open
    /// lazily later.
    ///
    /// The stake is not latency. `deadpool-sync` panics *unconditionally* when a
    /// connection **creation** is cancelled (a cancelled query, by contrast, it
    /// reports as an error), and the tokio runtime cancels queued blocking tasks
    /// as it shuts down. So a pool with connections still to open panics a worker
    /// on Ctrl-C — and shutdown is precisely when it would be opening them, since
    /// every actor flushes its state at once and a cold pool meets its first real
    /// contention there.
    ///
    /// Asserting the pool's size is the honest guard: a test that merely tore a
    /// runtime down and looked for the panic would pass either way, because the
    /// panic lands in a worker thread and never fails the test.
    #[tokio::test]
    async fn open_leaves_no_connection_to_be_created_later() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = SqlitePool::open(dir.path().join("t.db"))
            .await
            .expect("open");
        let status = pool.pool.status();
        assert_eq!(
            status.size, POOL_SIZE,
            "every connection must be open before the pool is handed out",
        );
        assert_eq!(status.available, POOL_SIZE, "and all of them idle");
    }

    /// Two pools must not see each other's data, or tests running in parallel
    /// would interfere through the shared cache.
    #[tokio::test]
    async fn in_memory_databases_are_isolated_from_each_other() {
        let a = SqlitePool::open_in_memory().await.expect("open a");
        let b = SqlitePool::open_in_memory().await.expect("open b");
        a.interact("test.write", |conn| {
            conn.execute(
                "INSERT INTO secrets (name, encrypted_value) VALUES ('k', x'01')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("write");

        let n: i64 = b
            .interact("test.read", |conn| {
                Ok(conn.query_row("SELECT count(*) FROM secrets", [], |r| r.get(0))?)
            })
            .await
            .expect("read");
        assert_eq!(n, 0, "pools must not share an in-memory database");
    }
}
