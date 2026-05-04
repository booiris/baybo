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
pub use skill_risk::LibsqlSkillRiskStore;
pub use trace::{LibsqlTraceEventStore, LibsqlTraceStore};

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
    /// All tables that support deletion carry a `deleted_at` column (Unix
    /// **microseconds**, NULL when the row is live). See `soft_delete`
    /// module rules. All other timestamp columns (`created_at`,
    /// `started_at`, etc.) are also Unix microseconds — round-trip via
    /// `libsql::time::{to_us, from_us}`. µs is finer than the millisecond
    /// granularity of typical web tooling so sub-ms ordering survives
    /// (useful for fast local tool spans), and `chrono::timestamp_micros`
    /// is infallible. API surfaces (HTTP / OpenAPI / web) re-encode as
    /// RFC3339 and don't expose raw µs.
    async fn init_db(&self) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    id                    TEXT PRIMARY KEY,
                    root_session_id       TEXT NOT NULL,
                    trigger_kind          TEXT NOT NULL,
                    parent_session_id     TEXT,
                    parent_job_id         TEXT,
                    lineage_kind          TEXT,
                    bound_soul_version    TEXT NOT NULL,
                    created_at            INTEGER NOT NULL,
                    last_active           INTEGER NOT NULL,
                    data                  TEXT NOT NULL,
                    deleted_at            INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_sessions_root
                    ON sessions(root_session_id) WHERE deleted_at IS NULL;
                CREATE INDEX IF NOT EXISTS idx_sessions_parent
                    ON sessions(parent_session_id, lineage_kind)
                    WHERE deleted_at IS NULL AND lineage_kind IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_sessions_last_active
                    ON sessions(last_active DESC) WHERE deleted_at IS NULL;

                CREATE TABLE IF NOT EXISTS memories (
                    id         TEXT PRIMARY KEY,
                    user_id    TEXT NOT NULL,
                    content    TEXT NOT NULL,
                    data       TEXT NOT NULL,
                    deleted_at INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_memories_user_id ON memories(user_id);

                CREATE TABLE IF NOT EXISTS secrets (
                    name            TEXT PRIMARY KEY,
                    encrypted_value BLOB NOT NULL,
                    deleted_at      INTEGER
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
                    cost_usd                        REAL    NOT NULL,
                    timestamp                       INTEGER NOT NULL,
                    -- Mirrors sessions.deleted_at of session_id. Null while
                    -- the originating session is live; populated when the
                    -- session is soft-deleted so cost UIs can render
                    -- 'source session deleted' without joining back.
                    originating_session_deleted_at  INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_cost_user_id ON cost_records(user_id);
                CREATE INDEX IF NOT EXISTS idx_cost_timestamp ON cost_records(timestamp);
                CREATE INDEX IF NOT EXISTS idx_cost_session ON cost_records(session_id);

                CREATE TABLE IF NOT EXISTS user_monthly_cost (
                    user_id     TEXT    NOT NULL,
                    month       TEXT    NOT NULL,
                    cost_usd    REAL    NOT NULL,
                    updated_at  INTEGER NOT NULL,
                    deleted_at  INTEGER,
                    PRIMARY KEY (user_id, month)
                );

                CREATE TABLE IF NOT EXISTS jobs (
                    id                       TEXT PRIMARY KEY,
                    session_id               TEXT NOT NULL,
                    parent_job_id            TEXT,
                    kind                     TEXT NOT NULL,
                    status_kind              TEXT NOT NULL,
                    effective_soul_version   TEXT NOT NULL,
                    has_verifier             INTEGER NOT NULL DEFAULT 0,
                    created_at               INTEGER NOT NULL,
                    started_at               INTEGER,
                    ended_at                 INTEGER,
                    data                     TEXT NOT NULL,
                    deleted_at               INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_jobs_session
                    ON jobs(session_id, created_at) WHERE deleted_at IS NULL;
                CREATE INDEX IF NOT EXISTS idx_jobs_status
                    ON jobs(status_kind) WHERE deleted_at IS NULL;
                CREATE INDEX IF NOT EXISTS idx_jobs_parent
                    ON jobs(parent_job_id) WHERE deleted_at IS NULL;

                CREATE TABLE IF NOT EXISTS job_transitions (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id     TEXT NOT NULL,
                    data       TEXT NOT NULL,
                    deleted_at INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_job_transitions_job_id ON job_transitions(job_id);

                CREATE TABLE IF NOT EXISTS job_verification_transitions (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id     TEXT NOT NULL,
                    data       TEXT NOT NULL,
                    deleted_at INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_job_verif_job_id
                    ON job_verification_transitions(job_id);

                CREATE TABLE IF NOT EXISTS steps (
                    id          TEXT PRIMARY KEY,
                    job_id      TEXT NOT NULL,
                    kind        TEXT NOT NULL,
                    started_at  INTEGER NOT NULL,
                    ended_at    INTEGER,
                    outcome     TEXT NOT NULL,
                    data        TEXT NOT NULL,
                    deleted_at  INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_steps_job
                    ON steps(job_id, started_at) WHERE deleted_at IS NULL;

                CREATE TABLE IF NOT EXISTS spans (
                    id              TEXT PRIMARY KEY,
                    step_id         TEXT NOT NULL,
                    kind            TEXT NOT NULL,
                    parallel_group  TEXT,
                    started_at      INTEGER NOT NULL,
                    ended_at        INTEGER,
                    outcome         TEXT NOT NULL,
                    data            TEXT NOT NULL,
                    deleted_at      INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_spans_step
                    ON spans(step_id, started_at) WHERE deleted_at IS NULL;
                -- Used by recover_half_open_spans at startup.
                CREATE INDEX IF NOT EXISTS idx_spans_half_open
                    ON spans(ended_at)
                    WHERE ended_at IS NULL AND deleted_at IS NULL;

                CREATE TABLE IF NOT EXISTS span_events (
                    span_id    TEXT    NOT NULL,
                    seq        INTEGER NOT NULL,
                    at         INTEGER NOT NULL,
                    kind       TEXT    NOT NULL,
                    data       TEXT    NOT NULL,
                    deleted_at INTEGER,
                    PRIMARY KEY (span_id, seq)
                );

                -- Append-only WAL log of step / span begin / end events
                -- plus job transitions. Recovery source of truth — survives
                -- crashes that left the columnar tables behind.
                CREATE TABLE IF NOT EXISTS trace_events (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id  TEXT    NOT NULL,
                    job_id      TEXT    NOT NULL,
                    at          INTEGER NOT NULL,
                    data        TEXT    NOT NULL,
                    deleted_at  INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_trace_events_session
                    ON trace_events(session_id, at) WHERE deleted_at IS NULL;

                CREATE TABLE IF NOT EXISTS cron_jobs (
                    id              TEXT    PRIMARY KEY,
                    user_id         TEXT    NOT NULL,
                    status          TEXT    NOT NULL,
                    -- Unix ms; 0 means 'no scheduled fire'
                    -- (replaces the empty-string sentinel from the prior
                    -- TEXT/RFC3339 schema).
                    next_trigger_at INTEGER NOT NULL DEFAULT 0,
                    data            TEXT    NOT NULL,
                    deleted_at      INTEGER
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
                    data                TEXT    NOT NULL,
                    deleted_at          INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_cron_executions_job_id ON cron_executions(job_id);
                CREATE INDEX IF NOT EXISTS idx_cron_executions_user_id ON cron_executions(user_id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_executions_dedup ON cron_executions(job_id, scheduled_fire_time);
                CREATE INDEX IF NOT EXISTS idx_cron_executions_status ON cron_executions(status);

                CREATE TABLE IF NOT EXISTS skill_risk_assessments (
                    skill_name   TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    level        TEXT NOT NULL,
                    rationale    TEXT NOT NULL,
                    model        TEXT NOT NULL,
                    assessed_at  INTEGER NOT NULL,
                    deleted_at   INTEGER,
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
                    deleted_at   INTEGER,
                    PRIMARY KEY (skill_name, content_hash)
                );
                CREATE INDEX IF NOT EXISTS idx_skill_risk_jobs_status
                    ON skill_risk_assessment_jobs(status);

                CREATE TABLE IF NOT EXISTS channel_sessions (
                    channel_type TEXT    NOT NULL,
                    user_id      TEXT    NOT NULL,
                    session_id   TEXT    NOT NULL,
                    created_at   INTEGER NOT NULL,
                    deleted_at   INTEGER,
                    PRIMARY KEY (channel_type, user_id)
                );
                CREATE INDEX IF NOT EXISTS idx_channel_sessions_session
                    ON channel_sessions(session_id) WHERE deleted_at IS NULL;

                CREATE TABLE IF NOT EXISTS channel_bots (
                    channel_type TEXT    NOT NULL,
                    bot_id       TEXT    NOT NULL,
                    created_at   INTEGER NOT NULL,
                    deleted_at   INTEGER,
                    PRIMARY KEY (channel_type, bot_id)
                );

                CREATE TABLE IF NOT EXISTS blobs (
                    blob_id           TEXT PRIMARY KEY,
                    mime_type         TEXT NOT NULL,
                    size              INTEGER NOT NULL,
                    uploader_identity TEXT,
                    read_token        TEXT,
                    created_at        INTEGER NOT NULL,
                    last_accessed_at  INTEGER NOT NULL,
                    deleted_at        INTEGER
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
                    deleted_at   INTEGER,
                    PRIMARY KEY (channel_type, bot_id, user_id)
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_channel_pairings_code
                    ON channel_pairings(code) WHERE deleted_at IS NULL;",
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize libsql schema: {e}"))?;
        Ok(())
    }
}
