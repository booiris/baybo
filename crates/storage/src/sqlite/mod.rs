mod cost;
mod job;
mod memory;
mod secret;
mod session;
mod trace;

pub use cost::SqliteCostStore;
pub use job::SqliteJobStore;
pub use memory::SqliteMemoryStore;
pub use secret::SqliteSecretStore;
pub use session::SqliteSessionStore;
pub use trace::SqliteTraceStore;

use std::sync::{Arc, Mutex};

use aura_core::AuraError;
use rusqlite::Connection;

/// A shared handle to a SQLite connection, safe for use across async tasks.
///
/// Since `rusqlite::Connection` is synchronous, we wrap it in a `std::sync::Mutex`
/// behind an `Arc`. Async trait methods use `tokio::task::spawn_blocking` to avoid
/// blocking the tokio runtime.
#[derive(Clone)]
pub struct SqlitePool {
    conn: Arc<Mutex<Connection>>,
}

impl SqlitePool {
    /// Open (or create) a SQLite database at the given path and initialise the schema.
    pub fn open(path: &str) -> aura_core::Result<Self> {
        let conn = Connection::open(path).map_err(|e| {
            AuraError::Config(format!("failed to open sqlite database at {path}: {e}"))
        })?;
        let pool = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        pool.init_db()?;
        Ok(pool)
    }

    /// Open an in-memory SQLite database (useful for tests).
    pub fn open_in_memory() -> aura_core::Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| {
            AuraError::Config(format!("failed to open in-memory sqlite database: {e}"))
        })?;
        let pool = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        pool.init_db()?;
        Ok(pool)
    }

    /// Acquire a lock on the underlying connection.
    fn lock(&self) -> aura_core::Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite mutex poisoned: {e}")))
    }

    /// Create all required tables if they do not already exist.
    fn init_db(&self) -> aura_core::Result<()> {
        let conn = self.lock()?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
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
            CREATE INDEX IF NOT EXISTS idx_cost_user_id   ON cost_records(user_id);
            CREATE INDEX IF NOT EXISTS idx_cost_timestamp  ON cost_records(timestamp);

            CREATE TABLE IF NOT EXISTS jobs (
                id   TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS job_transitions (
                id     INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id TEXT NOT NULL,
                data   TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_job_transitions_job_id ON job_transitions(job_id);
            ",
        )
        .map_err(|e| {
            AuraError::Internal(anyhow::anyhow!("failed to initialise sqlite schema: {e}"))
        })?;
        Ok(())
    }
}
