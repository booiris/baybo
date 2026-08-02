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

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Semaphore;

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
    inner: Arc<PoolInner>,
}

/// The connections, and the gate that hands them out one at a time.
struct PoolInner {
    /// Where a replacement connection comes from.
    path: String,
    /// Connections parked while nobody holds them. A plain mutex is right here:
    /// the critical section is a `Vec` push or pop, never a query.
    idle: Mutex<Vec<rusqlite::Connection>>,
    /// One permit per connection, so a caller arriving at a fully checked-out
    /// pool waits here instead of finding `idle` empty. `Arc` because the permit
    /// outlives this borrow: it rides into the blocking task and is released
    /// only once the connection is back.
    permits: Arc<Semaphore>,
}

impl PoolInner {
    /// Take the connection this caller's permit entitles it to.
    ///
    /// `idle` is empty only when the previous holder's closure panicked and
    /// unwound with the connection, so opening a replacement — rather than
    /// failing, or handing back one the panic may have left mid-statement — is
    /// what keeps "one permit, one connection" true.
    fn take(&self) -> Result<rusqlite::Connection, SqliteError> {
        match self.idle.lock().pop() {
            Some(conn) => Ok(conn),
            None => open_connection(&self.path),
        }
    }

    fn give_back(&self, conn: rusqlite::Connection) {
        self.idle.lock().push(conn);
    }
}

/// Open one connection and put it in the state every connection must be in.
///
/// `journal_mode` is persisted in the file header and only needs saying once,
/// but `synchronous` and `busy_timeout` are per-handle and would otherwise
/// silently sit at sqlite's defaults. WAL is what lets the dashboard read while
/// the traffic flush writes.
fn open_connection(path: &str) -> Result<rusqlite::Connection, SqliteError> {
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

impl SqlitePool {
    /// Open every connection up front, rather than lazily on first contention.
    ///
    /// Opening is I/O, and first contention is by definition the moment the
    /// process can least afford to pay for it. It also makes a database that
    /// cannot supply [`POOL_SIZE`] handles fail here rather than mid-query later.
    pub(crate) async fn open(path: &str) -> Result<Self, SqliteError> {
        let owned = path.to_string();
        let connections = tokio::task::spawn_blocking(move || {
            (0..POOL_SIZE)
                .map(|_| open_connection(&owned))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(|e| SqliteError::Pool(e.to_string()))??;
        Ok(Self {
            inner: Arc::new(PoolInner {
                path: path.to_string(),
                idle: Mutex::new(connections),
                permits: Arc::new(Semaphore::new(POOL_SIZE)),
            }),
        })
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
        let permit = self
            .inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| SqliteError::Pool(e.to_string()))?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            // Moved in so it is released on the blocking thread, after the
            // connection is back in `idle` — a permit handed on while the
            // connection is still out would admit a caller with nothing to take.
            // Declared first so it also drops last.
            let _permit = permit;
            let mut conn = inner.take()?;
            let out = f(&mut conn).map_err(SqliteError::Sqlite);
            inner.give_back(conn);
            out
        })
        .await
        // The closure panicked, so it never produced a result. Its connection
        // went with it rather than returning to the pool possibly mid-statement.
        .map_err(|e| SqliteError::Pool(e.to_string()))?
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

    /// An open pool must already hold every connection, leaving none to open
    /// lazily later.
    ///
    /// The one path that opens a connection after `open` — the replacement
    /// branch in [`PoolInner::take`] — exists solely to recover from a panicking
    /// closure and must stay unreached in ordinary service. Asserting the idle
    /// count is what pins that down: a pool that filled itself on demand would
    /// pass every other test here.
    #[tokio::test]
    async fn open_leaves_no_connection_to_be_created_later() {
        let path = TempDbPath::new();
        let pool = SqlitePool::open(&path.0).await.expect("open");
        assert_eq!(
            pool.inner.idle.lock().len(),
            POOL_SIZE,
            "every connection must be open before the pool is handed out",
        );
        assert_eq!(
            pool.inner.permits.available_permits(),
            POOL_SIZE,
            "and all of them idle"
        );
    }

    /// A panicking closure takes its connection down with it — the handle may be
    /// mid-statement, so it is dropped rather than returned. What must not go
    /// with it is the *permit*: leak one per panic and the pool silently loses a
    /// slot each time, until a caller waits on a semaphore that will never be
    /// posted. The concurrent burst below is the assertion — it can only finish
    /// if all [`POOL_SIZE`] slots came back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_panicking_closure_costs_the_pool_no_capacity() {
        let path = TempDbPath::new();
        let pool = SqlitePool::open(&path.0).await.expect("open");
        pool.interact(|conn| conn.execute_batch("CREATE TABLE t (k TEXT NOT NULL)"))
            .await
            .expect("seed");

        // One more than the pool holds, so the last is served by a replacement
        // connection rather than an original.
        for _ in 0..POOL_SIZE + 1 {
            pool.interact(|_| -> rusqlite::Result<()> { panic!("closure blew up") })
                .await
                .expect_err("a panicking closure must surface as an error, not a value");
        }

        let mut tasks = Vec::new();
        for w in 0..POOL_SIZE {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                pool.interact(move |conn| {
                    conn.execute(
                        "INSERT INTO t (k) VALUES (?1)",
                        rusqlite::params![format!("k{w}")],
                    )
                })
                .await
                .expect("the pool still serves every slot after a panic");
            }));
        }
        for t in tasks {
            t.await.expect("writer panicked");
        }

        assert!(
            pool.inner.idle.lock().len() <= POOL_SIZE,
            "replacements must not grow the pool past its capacity"
        );
    }

    /// A PRAGMA that fails to apply looks exactly like one that applied — the
    /// query still runs, just with the old setting. Assert the connection's
    /// actual state, on enough connections to cover the pool.
    #[tokio::test]
    async fn every_connection_gets_the_pragmas() {
        let path = TempDbPath::new();
        let pool = SqlitePool::open(&path.0).await.expect("open");

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
        let pool = SqlitePool::open(&path.0).await.expect("open");
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
