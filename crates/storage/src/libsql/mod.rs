mod blob;
mod channel_bot;
mod channel_pairing;
mod channel_session;
mod cost;
mod cron;
mod device;
mod job;
mod secret;
mod session;
mod session_folder;
mod session_summary;
mod skill_risk;
mod task;
mod time;
mod trace;

pub use blob::LibsqlBlobStore;
pub use channel_bot::LibsqlChannelBotStore;
pub use channel_pairing::LibsqlChannelPairingStore;
pub use channel_session::LibsqlChannelSessionStore;
pub use cost::LibsqlCostStore;
pub use cron::LibsqlCronStore;
pub use device::LibsqlDeviceStore;
pub use job::LibsqlJobStore;
pub use secret::LibsqlSecretStore;
pub use session::LibsqlSessionStore;
pub use session_folder::LibsqlSessionFolderStore;
pub use session_summary::LibsqlSessionSummaryStore;
pub use skill_risk::LibsqlSkillRiskStore;
pub use task::LibsqlTaskStore;
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
    /// the JSON `data` blob via `json_extract`. `baybo-trace` serialises
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
                    PRIMARY KEY (session_id, ordinal)
                );
                CREATE INDEX IF NOT EXISTS idx_session_messages_active
                    ON session_messages(session_id, ordinal)
                    WHERE superseded_by IS NULL;

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
                CREATE INDEX IF NOT EXISTS idx_blobs_uploader_identity_size
                    ON blobs(uploader_identity, size)
                    WHERE uploader_identity IS NOT NULL;

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
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize libsql schema: {e}"))?;

        // Migrations for DBs created before a column was added. libsql
        // / SQLite has no `ADD COLUMN IF NOT EXISTS`, so we attempt the
        // ALTER and swallow the "duplicate column" error. Add new
        // migrations to this list rather than mutating the CREATE
        // TABLE — fresh DBs pick the column up from CREATE, existing
        // DBs from the ALTER.
        let migrations: &[&str] = &[
            "ALTER TABLE sessions ADD COLUMN parent_span_id TEXT",
            "ALTER TABLE sessions ADD COLUMN last_llm TEXT",
            "ALTER TABLE sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE cost_records ADD COLUMN reason TEXT",
            "ALTER TABLE sessions ADD COLUMN folder_id TEXT",
            "ALTER TABLE devices ADD COLUMN relay_url TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE devices ADD COLUMN remote_api_key TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE session_messages ADD COLUMN platform_msg_id TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
        ];
        for stmt in migrations {
            if let Err(e) = self.conn.execute(stmt, libsql::params![]).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(anyhow::anyhow!("migration `{stmt}` failed: {msg}"));
                }
            }
        }

        // Index on the migration-added `sessions.folder_id` column. Created
        // AFTER the ALTER loop, not in the schema batch above: on a legacy DB
        // the column doesn't exist until the ALTER runs, so a batch-time
        // CREATE INDEX referencing it would fail. `IF NOT EXISTS` keeps it
        // idempotent on every subsequent boot.
        self.conn
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_folder \
                 ON sessions(folder_id) WHERE folder_id IS NOT NULL",
                libsql::params![],
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to create idx_sessions_folder: {e}"))?;

        Ok(())
    }
}
