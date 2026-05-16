mod blob;
mod channel_bot;
mod channel_pairing;
mod channel_session;
mod cost;
mod cron;
mod job;
mod memory;
mod secret;
mod session;
mod session_summary;
mod skill_risk;
mod time;
mod trace;

pub use blob::LibsqlBlobStore;
pub use channel_bot::LibsqlChannelBotStore;
pub use channel_pairing::LibsqlChannelPairingStore;
pub use channel_session::LibsqlChannelSessionStore;
pub use cost::LibsqlCostStore;
pub use cron::LibsqlCronStore;
pub use job::LibsqlJobStore;
pub use memory::LibsqlMemoryStore;
pub use secret::LibsqlSecretStore;
pub use session::LibsqlSessionStore;
pub use session_summary::LibsqlSessionSummaryStore;
pub use skill_risk::LibsqlSkillRiskStore;
pub use trace::LibsqlTraceStore;

use std::sync::Arc;

/// Shared handle to a libsql database connection.
///
/// Wraps a single `libsql::Connection` behind an `Arc` so it can be
/// cheaply cloned and shared across async tasks.
#[derive(Clone)]
pub struct LibsqlPool {
    conn: Arc<libsql::Connection>,
}

impl LibsqlPool {
    /// Open (or create) a local libsql database at the given path.
    pub async fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", parent.display()))?;
        }
        let db = libsql::Builder::new_local(path)
            .build()
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to open libsql database at {}: {e}", path.display())
            })?;
        let conn = db
            .connect()
            .map_err(|e| anyhow::anyhow!("failed to get libsql connection: {e}"))?;
        let pool = Self {
            conn: Arc::new(conn),
        };
        pool.set_wal_mode().await?;
        pool.init_db().await?;
        Ok(pool)
    }

    /// Open an in-memory libsql database.
    pub async fn open_in_memory() -> anyhow::Result<Self> {
        let db = libsql::Builder::new_local(":memory:")
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("failed to open in-memory libsql database: {e}"))?;
        let conn = db
            .connect()
            .map_err(|e| anyhow::anyhow!("failed to get in-memory libsql connection: {e}"))?;
        let pool = Self {
            conn: Arc::new(conn),
        };
        pool.init_db().await?;
        Ok(pool)
    }

    /// Get a reference to the underlying connection.
    pub(crate) fn conn(&self) -> &libsql::Connection {
        &self.conn
    }

    /// Enable WAL journaling so writers no longer churn a `-journal` sidecar
    /// on every transaction. `synchronous=NORMAL` is the recommended pairing
    /// for WAL (crash-safe, faster than FULL).
    async fn set_wal_mode(&self) -> anyhow::Result<()> {
        self.conn
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .await
            .map_err(|e| anyhow::anyhow!("failed to enable WAL mode: {e}"))?;
        Ok(())
    }

    /// Create all required tables if they do not already exist.
    ///
    /// Timestamp columns (`created_at`, `started_at` on `jobs`, etc.) are Unix
    /// microseconds — round-trip via `libsql::time::{to_us, from_us}`. µs is
    /// finer than the millisecond granularity of typical web tooling so sub-ms
    /// ordering survives (useful for fast local tool spans), and
    /// `chrono::timestamp_micros` is infallible. API surfaces (HTTP /
    /// OpenAPI / web) re-encode as RFC3339 and don't expose raw µs.
    ///
    /// **Exception — trace tables (`steps`, `spans`).** The `started_at`
    /// / `ended_at` columns are TEXT generated columns extracted from
    /// the JSON `data` blob via `json_extract`. `aura-trace` serialises
    /// `chrono::DateTime<Utc>` as RFC3339 strings, so these columns
    /// hold RFC3339 — sortable lexicographically because the leading
    /// `YYYY-MM-DDTHH:MM:SS` prefix is fixed-width and any
    /// fractional-second suffix shares a common prefix length within
    /// a single insertion path. They don't follow the µs invariant
    /// the rest of the schema uses; querying these columns means
    /// string comparison, not integer comparison.
    async fn init_db(&self) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
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
                    -- non-subagent lineage (maintenance / fork) and
                    -- for sessions migrated from before this column
                    -- existed.
                    parent_span_id        TEXT,
                    lineage_kind          TEXT,
                    bound_soul_version    TEXT NOT NULL,
                    created_at            INTEGER NOT NULL,
                    last_active           INTEGER NOT NULL,
                    -- 1 for user-facing sessions (the default); 0 for internal
                    -- maintenance sessions (e.g. SystemReason::BackgroundCompression).
                    -- Default `SessionStore` listings filter `is_normal_session = 1`
                    -- so maintenance sessions stay invisible in CLI / UI session
                    -- pickers; opt-in helpers exist for the spawn-serialization
                    -- lookup and the orphan reaper.
                    is_normal_session     INTEGER NOT NULL DEFAULT 1,
                    -- User-facing chat-list hide flag, set by
                    -- DELETE /v1/chat/sessions/:id. Filtered at the
                    -- chat API layer only; SessionStore::list_all
                    -- does NOT exclude hidden rows, so admin / trace
                    -- surfaces still see them.
                    hidden                INTEGER NOT NULL DEFAULT 0,
                    data                  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_sessions_root
                    ON sessions(root_session_id);
                CREATE INDEX IF NOT EXISTS idx_sessions_parent
                    ON sessions(parent_session_id, lineage_kind)
                    WHERE lineage_kind IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_sessions_parent_span
                    ON sessions(parent_span_id)
                    WHERE parent_span_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_sessions_last_active
                    ON sessions(last_active DESC);
                -- Partial index over normal sessions only — most listings hit
                -- this path, and excluding maintenance keeps the index narrow.
                CREATE INDEX IF NOT EXISTS idx_sessions_normal_last_active
                    ON sessions(last_active DESC)
                    WHERE is_normal_session = 1;
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
                    -- 1 only when this row originated from a direct user
                    -- channel input. The agent itself appends several
                    -- `role = 'user'` rows (skill reminders, system-
                    -- reminders); this column distinguishes the genuine
                    -- prompt so trace replay can surface it as the job's
                    -- user input rather than guessing by content.
                    from_user     INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (session_id, ordinal)
                );
                CREATE INDEX IF NOT EXISTS idx_session_messages_active
                    ON session_messages(session_id, ordinal)
                    WHERE superseded_by IS NULL;

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
                    -- 1 while a `BackgroundCompressionRunner` pass is active for
                    -- this parent; 0 otherwise. The trigger gate reads this
                    -- column to enforce the at-most-one-in-flight invariant
                    -- without inspecting the maintenance session row (which is
                    -- preserved as audit history). Set by the gate before
                    -- emitting a `SystemSpawnRequest`; cleared by
                    -- `record_summary_success`/`record_summary_failure` and by
                    -- the orphan reaper.
                    in_flight   INTEGER NOT NULL DEFAULT 0,
                    -- Opaque owner token (UUID) stamped by the trigger gate
                    -- when it sets `in_flight = 1`. The runner's defensive
                    -- post-pass cleanup uses a CAS-style clear (UPDATE
                    -- `in_flight_owner = ?`) so a Pass A that finishes
                    -- *after* a Pass B already marked itself in flight
                    -- cannot wipe Pass B's mark. Reset to NULL by every
                    -- terminal handler (`upsert_success` /
                    -- `bump_error_count` / `clear_all_in_flight`) so a
                    -- newly-started pass starts from a clean slate.
                    in_flight_owner TEXT
                );

                CREATE TABLE IF NOT EXISTS memories (
                    id         TEXT PRIMARY KEY,
                    user_id    TEXT NOT NULL,
                    content    TEXT NOT NULL,
                    data       TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_memories_user_id ON memories(user_id);

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
                CREATE INDEX IF NOT EXISTS idx_cost_timestamp ON cost_records(timestamp);
                CREATE INDEX IF NOT EXISTS idx_cost_session ON cost_records(session_id);
                CREATE INDEX IF NOT EXISTS idx_cost_job ON cost_records(job_id);

                CREATE TABLE IF NOT EXISTS jobs (
                    id                       TEXT PRIMARY KEY,
                    session_id               TEXT NOT NULL,
                    parent_job_id            TEXT,
                    kind                     TEXT NOT NULL,
                    status_kind              TEXT NOT NULL,
                    effective_soul_version   TEXT NOT NULL,
                    created_at               INTEGER NOT NULL,
                    started_at               INTEGER,
                    ended_at                 INTEGER,
                    data                     TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_jobs_session
                    ON jobs(session_id, created_at);
                CREATE INDEX IF NOT EXISTS idx_jobs_status
                    ON jobs(status_kind);
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
                -- `aura-trace`, no schema migration.
                CREATE TABLE IF NOT EXISTS steps (
                    id         TEXT PRIMARY KEY,
                    data       TEXT NOT NULL,
                    job_id     TEXT GENERATED ALWAYS AS (json_extract(data, '$.job_id')) VIRTUAL,
                    started_at TEXT GENERATED ALWAYS AS (json_extract(data, '$.started_at')) VIRTUAL,
                    ended_at   TEXT GENERATED ALWAYS AS (json_extract(data, '$.ended_at')) VIRTUAL
                );
                CREATE INDEX IF NOT EXISTS idx_steps_job
                    ON steps(job_id, started_at);

                CREATE TABLE IF NOT EXISTS spans (
                    id         TEXT PRIMARY KEY,
                    data       TEXT NOT NULL,
                    step_id    TEXT GENERATED ALWAYS AS (json_extract(data, '$.step_id')) VIRTUAL,
                    started_at TEXT GENERATED ALWAYS AS (json_extract(data, '$.started_at')) VIRTUAL,
                    ended_at   TEXT GENERATED ALWAYS AS (json_extract(data, '$.ended_at')) VIRTUAL
                );
                CREATE INDEX IF NOT EXISTS idx_spans_step
                    ON spans(step_id, started_at);

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
                    ON channel_pairings(code);",
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize libsql schema: {e}"))?;

        // Migrations for DBs created before a column was added. libsql
        // / SQLite has no `ADD COLUMN IF NOT EXISTS`, so we attempt the
        // ALTER and swallow the "duplicate column" error. Add new
        // migrations to this list rather than mutating the CREATE
        // TABLE — fresh DBs pick the column up from CREATE, existing
        // DBs from the ALTER.
        let migrations: &[&str] = &["ALTER TABLE sessions ADD COLUMN parent_span_id TEXT"];
        for stmt in migrations {
            if let Err(e) = self.conn.execute(stmt, libsql::params![]).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(anyhow::anyhow!("migration `{stmt}` failed: {msg}"));
                }
            }
        }

        Ok(())
    }
}
