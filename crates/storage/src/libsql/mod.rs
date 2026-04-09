mod cost;
mod job;
mod memory;
mod secret;
mod session;
mod trace;

pub use cost::LibsqlCostStore;
pub use job::LibsqlJobStore;
pub use memory::LibsqlMemoryStore;
pub use secret::LibsqlSecretStore;
pub use session::LibsqlSessionStore;
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
    pub async fn open(path: &str) -> anyhow::Result<Self> {
        let db = libsql::Builder::new_local(path)
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("failed to open libsql database at {path}: {e}"))?;
        let conn = db
            .connect()
            .map_err(|e| anyhow::anyhow!("failed to get libsql connection: {e}"))?;
        let pool = Self {
            conn: Arc::new(conn),
        };
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

    /// Create all required tables if they do not already exist.
    async fn init_db(&self) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    id   TEXT PRIMARY KEY,
                    data TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS memories (
                    id      TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    data    TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_memories_user_id ON memories(user_id);

                CREATE TABLE IF NOT EXISTS session_traces (
                    session_id TEXT PRIMARY KEY,
                    data       TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS trace_nodes (
                    id         TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    data       TEXT NOT NULL,
                    PRIMARY KEY (session_id, id)
                );

                CREATE TABLE IF NOT EXISTS secrets (
                    name            TEXT PRIMARY KEY,
                    encrypted_value BLOB NOT NULL
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
                    id   TEXT PRIMARY KEY,
                    data TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS job_transitions (
                    id     INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id TEXT NOT NULL,
                    data   TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_job_transitions_job_id ON job_transitions(job_id);",
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize libsql schema: {e}"))?;
        Ok(())
    }
}
