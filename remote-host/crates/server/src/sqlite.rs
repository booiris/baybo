//! The sqlite connection pool the admission allow-list and the traffic ledger
//! both sit on.
//!
//! A connection is checked out **exclusively for the whole closure** — prepare,
//! step, *and every `row.get()`*. That is a memory-safety contract, not a
//! throughput knob. A sqlite connection owns an unsynchronised private heap (its
//! lookaside allocator), and the C API mutates it while *decoding*, not only
//! while querying: `sqlite3_value_text()` allocates in order to NUL-terminate a
//! TEXT column. Two threads inside one handle therefore corrupt the free list,
//! and the process dies later in an unrelated allocation. A lock around only the
//! query would be a non-fix.
//!
//! Sharing one handle is easy to do by accident — the dashboard fetches several
//! traffic series at once, so its handlers run in parallel on different workers.
//! `rusqlite::Connection` is `Send` but deliberately **not** `Sync`, so the
//! compiler, not a convention, is what keeps them out.

use std::time::Duration;

use deadpool_sqlite::{Config, Runtime};

/// Readers never block each other under WAL and writers serialise on the write
/// lock regardless, so this only needs to cover the dashboard's concurrent
/// handlers plus the traffic flush.
const POOL_SIZE: usize = 4;

/// A second writer waits rather than failing: the traffic flush and a dashboard
/// mutation legitimately overlap, and sqlite's default of 0 would turn that into
/// a spurious `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(crate) enum SqliteError {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("pool: {0}")]
    Pool(String),
}

#[derive(Clone)]
pub(crate) struct SqlitePool {
    pool: deadpool_sqlite::Pool,
}

impl SqlitePool {
    pub(crate) fn open(path: &str) -> Result<Self, SqliteError> {
        let pool = Config::new(path)
            .builder(Runtime::Tokio1)
            .map_err(|e| SqliteError::Pool(e.to_string()))?
            .max_size(POOL_SIZE)
            // Per-connection state, so it belongs on the hook that fires for every
            // connection the pool ever creates — including ones opened lazily
            // under load, or replaced after a recycle. `journal_mode` is persisted
            // in the file header and only needs saying once, but `synchronous` and
            // `busy_timeout` are per-handle and would otherwise silently fall back
            // to sqlite's defaults on a fresh handle.
            //
            // WAL is what lets the dashboard read while the traffic flush writes.
            .post_create(deadpool_sqlite::Hook::async_fn(|conn, _| {
                Box::pin(async move {
                    conn.interact(|conn| {
                        conn.busy_timeout(BUSY_TIMEOUT)?;
                        conn.pragma_update(None, "journal_mode", "WAL")?;
                        conn.pragma_update(None, "synchronous", "NORMAL")
                    })
                    .await
                    .map_err(|e| deadpool_sqlite::HookError::message(e.to_string()))?
                    .map_err(deadpool_sqlite::HookError::Backend)
                })
            }))
            .build()
            .map_err(|e| SqliteError::Pool(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Run `f` against a connection held exclusively for the whole closure.
    ///
    /// `f` runs on a blocking thread (rusqlite is synchronous), so it must own
    /// its inputs — bind every parameter as an owned value.
    pub(crate) async fn interact<F, T>(&self, f: F) -> Result<T, SqliteError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| SqliteError::Pool(e.to_string()))?;
        conn.interact(f)
            .await
            // The closure panicked, or a previous one did and poisoned the
            // connection. Either way it never produced a result.
            .map_err(|e| SqliteError::Pool(e.to_string()))?
            .map_err(SqliteError::Sqlite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp on-disk DB path that removes the `.db`/`-wal`/`-shm` sidecars on drop.
    struct TempDbPath(String);
    impl TempDbPath {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("sqlite-pool-{}.db", rand::random::<u64>()))
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }
    impl Drop for TempDbPath {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0));
            }
        }
    }

    /// A PRAGMA that fails to apply looks exactly like one that applied — the
    /// query still runs, just with the old setting. Assert the connection's
    /// actual state, on enough connections to cover the pool.
    #[tokio::test]
    async fn every_connection_gets_the_pragmas() {
        let path = TempDbPath::new();
        let pool = SqlitePool::open(&path.0).expect("open");

        for _ in 0..POOL_SIZE * 2 {
            let (journal_mode, busy_timeout) = pool
                .interact(|conn| {
                    Ok((
                        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
                        conn.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, i64>(0))?,
                    ))
                })
                .await
                .expect("read pragmas");
            assert_eq!(journal_mode, "wal");
            assert_eq!(busy_timeout, BUSY_TIMEOUT.as_millis() as i64);
        }
    }

    /// The shape that segfaulted the gateway: the dashboard fetching several
    /// series at once means concurrent handlers decoding TEXT columns. Under a
    /// driver that lets them share one handle this dies with a signal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_text_decoding_does_not_corrupt_the_connection() {
        let path = TempDbPath::new();
        let pool = SqlitePool::open(&path.0).expect("open");
        pool.interact(|conn| {
            conn.execute_batch("CREATE TABLE t (k TEXT NOT NULL, v INTEGER NOT NULL)")?;
            let tx = conn.transaction()?;
            for i in 0..200 {
                tx.execute(
                    "INSERT INTO t (k, v) VALUES (?1, ?2)",
                    rusqlite::params![format!("remote_api_key-{i}-{}", "x".repeat(64)), i],
                )?;
            }
            tx.commit()
        })
        .await
        .expect("seed");

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..50 {
                    let rows: Vec<(String, i64)> = pool
                        .interact(|conn| {
                            let mut stmt = conn.prepare("SELECT k, v FROM t ORDER BY v")?;
                            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                                .collect::<rusqlite::Result<Vec<_>>>()
                        })
                        .await
                        .expect("read");
                    assert_eq!(rows.len(), 200);
                }
            }));
        }
        for t in tasks {
            t.await.expect("reader panicked");
        }
    }
}
