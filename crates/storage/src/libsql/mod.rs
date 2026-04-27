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
    /// All tables that support deletion carry a `deleted_at` column (Unix
    /// seconds, NULL when the row is live). See `soft_delete` module rules.
    async fn init_db(&self) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    id         TEXT PRIMARY KEY,
                    data       TEXT NOT NULL,
                    deleted_at INTEGER
                );

                CREATE TABLE IF NOT EXISTS memories (
                    id         TEXT PRIMARY KEY,
                    user_id    TEXT NOT NULL,
                    content    TEXT NOT NULL,
                    data       TEXT NOT NULL,
                    deleted_at INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_memories_user_id ON memories(user_id);

                CREATE TABLE IF NOT EXISTS session_traces (
                    session_id  TEXT PRIMARY KEY,
                    root_node   TEXT NOT NULL,
                    active_leaf TEXT NOT NULL,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    deleted_at  INTEGER
                );

                CREATE TABLE IF NOT EXISTS trace_nodes (
                    id         TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    parent_id  TEXT,
                    kind       TEXT NOT NULL,
                    job_id     TEXT,
                    provenance TEXT NOT NULL DEFAULT '{}',
                    input      TEXT NOT NULL DEFAULT '{}',
                    result     TEXT,
                    started_at TEXT NOT NULL,
                    ended_at   TEXT,
                    snapshot   TEXT,
                    deleted_at INTEGER,
                    PRIMARY KEY (session_id, id)
                );
                CREATE INDEX IF NOT EXISTS idx_trace_nodes_started
                    ON trace_nodes(session_id, started_at);

                CREATE TABLE IF NOT EXISTS trace_forks (
                    id         TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    from_node  TEXT NOT NULL,
                    fork_root  TEXT NOT NULL,
                    reason     TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    deleted_at INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_trace_forks_session
                    ON trace_forks(session_id);

                CREATE TABLE IF NOT EXISTS secrets (
                    name            TEXT PRIMARY KEY,
                    encrypted_value BLOB NOT NULL,
                    deleted_at      INTEGER
                );

                CREATE TABLE IF NOT EXISTS cost_records (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id       TEXT    NOT NULL,
                    session_id    TEXT    NOT NULL,
                    job_id        TEXT    NOT NULL,
                    trace_span_id TEXT    NOT NULL,
                    model         TEXT    NOT NULL,
                    input_tokens  INTEGER NOT NULL,
                    output_tokens INTEGER NOT NULL,
                    cost_usd      REAL    NOT NULL,
                    timestamp     TEXT    NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_cost_user_id ON cost_records(user_id);
                CREATE INDEX IF NOT EXISTS idx_cost_timestamp ON cost_records(timestamp);

                CREATE TABLE IF NOT EXISTS jobs (
                    id         TEXT PRIMARY KEY,
                    data       TEXT NOT NULL,
                    deleted_at INTEGER
                );

                CREATE TABLE IF NOT EXISTS job_transitions (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id     TEXT NOT NULL,
                    data       TEXT NOT NULL,
                    deleted_at INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_job_transitions_job_id ON job_transitions(job_id);

                CREATE TABLE IF NOT EXISTS cron_jobs (
                    id              TEXT PRIMARY KEY,
                    user_id         TEXT NOT NULL,
                    status          TEXT NOT NULL,
                    next_trigger_at TEXT NOT NULL DEFAULT '',
                    data            TEXT NOT NULL,
                    deleted_at      INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_cron_jobs_user_id ON cron_jobs(user_id);
                CREATE INDEX IF NOT EXISTS idx_cron_jobs_due ON cron_jobs(status, next_trigger_at);

                CREATE TABLE IF NOT EXISTS cron_executions (
                    id                  TEXT PRIMARY KEY,
                    job_id              TEXT NOT NULL,
                    user_id             TEXT NOT NULL,
                    scheduled_fire_time TEXT NOT NULL DEFAULT '',
                    triggered_at        TEXT NOT NULL,
                    status              TEXT NOT NULL DEFAULT 'pending',
                    data                TEXT NOT NULL,
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
