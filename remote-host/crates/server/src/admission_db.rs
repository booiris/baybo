//! The admission allow-list, sourced from a SQLite table and hot-reloaded by
//! polling — the table may be updated out of band (another process writes it), so
//! the runtime re-reads it on an interval rather than reacting to in-process
//! writes.
//!
//! [`AdmissionDb`] also owns the in-process **write path** the operator dashboard
//! drives (admit / edit / revoke / reveal / list). Writes go through the same
//! [`SqlitePool`], and the [`AdmissionDb::force_reload`] that follows each one
//! reloads the in-memory view read-after-write, firing the shared `on_revoke`
//! hook for any key that just lost admission.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use remote_host_admission::{AdmissionEntry, InMemoryAdmission};
use remote_host_protocol::key_tag;
use rusqlite::{OptionalExtension, params};

use crate::sqlite::{SqliteError, SqlitePool};

/// Source-of-truth table: one row per admitted `remote_api_key`. `label` +
/// `created_at` are for whoever administers it. `max_conns` + `max_bps` are
/// **required** (a key must declare its own limits) — enforced by the `CHECK`;
/// `per_server_max_bps` is optional (NULL → falls back to the row's `max_bps`).
/// `expires_at` is an optional wall-clock expiry (NULL → never expires).
///
/// The `CHECK` only guards a freshly-created table; `CREATE TABLE IF NOT EXISTS`
/// can't retrofit it onto a DB made under an older schema.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS remote_api_keys (\
    remote_api_key TEXT PRIMARY KEY, \
    label TEXT, \
    max_conns INTEGER, \
    max_bps INTEGER, \
    per_server_max_bps INTEGER, \
    expires_at TEXT, \
    created_at TEXT NOT NULL DEFAULT (datetime('now')), \
    CHECK (max_conns IS NOT NULL AND max_bps IS NOT NULL))";

/// Keep an admitted-but-expired key out of the in-memory view: drop a row only
/// when it carries an `expires_at` and that instant has passed. NULL expiry is
/// always kept.
const NOT_EXPIRED: &str = "NOT (expires_at IS NOT NULL AND expires_at < datetime('now'))";

/// Generated-key shape (per the dashboard contract): `"rh_" + hex(32 random bytes)`.
const GENERATED_KEY_BYTES: usize = 32;
const GENERATED_KEY_PREFIX: &str = "rh_";

/// Errors raised by the admission write/read path. Reads/writes flow through the
/// shared sqlite pool; a PK collision on admit surfaces here as [`Self::Db`] and is
/// mapped to a 409 by the dashboard backend.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionDbError {
    #[error("admission db: {0}")]
    Db(#[from] SqliteError),
    #[error("remote_api_key not found")]
    NotFound,
    #[error("a key requires both max_conns and max_bps")]
    MissingLimits,
    #[error("limit {field} value out of range")]
    LimitOutOfRange { field: &'static str },
}

/// Called with the keys that just lost admission on every reload (poll tick or
/// [`AdmissionDb::force_reload`]) so their live connections + bandwidth buckets can
/// be dropped. Shared by the poller task and the write path's reload, hence `Arc`.
pub(crate) type RevokeHook = Arc<dyn Fn(HashSet<String>) + Send + Sync>;

/// The admission allow-list handle: the live in-memory view plus the pool the
/// dashboard's mutations run on. The poller task (spawned by [`open`]) holds its own
/// clone of the pool, so it reads on a different connection than a concurrent write.
pub(crate) struct AdmissionDb {
    admission: Arc<InMemoryAdmission>,
    pool: SqlitePool,
    on_revoke: RevokeHook,
}

/// Open the DB at `path`, ensure the table, load the allow-list, and spawn a task
/// that re-reads it every `poll` to pick up external edits (on its own pooled
/// connection — the WAL win). On each reload, `on_revoke` is called with the keys
/// that just lost admission so their live connections can be dropped. Returns a
/// handle owning the pool + shared admission view.
pub(crate) async fn open(
    path: &str,
    poll: Duration,
    on_revoke: RevokeHook,
) -> Result<AdmissionDb, AdmissionDbError> {
    let pool = SqlitePool::open(path)?;
    pool.interact(|conn| conn.execute_batch(SCHEMA)).await?;

    let admission = Arc::new(InMemoryAdmission::new());
    // Initial load: nothing was admitted before, so nothing to revoke.
    let initial = load(&pool).await?;
    let keys_admitted = initial.len();
    let _ = admission.replace_all(initial);
    tracing::info!(
        keys_admitted,
        path,
        poll_secs = poll.as_secs(),
        "remote-host: admission allow-list loaded"
    );
    if keys_admitted == 0 {
        tracing::warn!(
            path,
            "remote-host: admission allow-list is EMPTY — every relay connection will be refused; admit keys via the dashboard"
        );
    }

    let poll_pool = pool.clone();
    let admission_poll = admission.clone();
    let on_revoke_poll = on_revoke.clone();
    let poll_path = path.to_owned();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(poll);
        tick.tick().await; // the first tick is immediate; we already loaded once
        let mut consecutive_failures: u32 = 0;
        loop {
            tick.tick().await;
            match load(&poll_pool).await {
                Ok(keys) => {
                    if consecutive_failures > 0 {
                        tracing::info!(
                            path = %poll_path,
                            consecutive_failures,
                            "remote-host: admission poll recovered"
                        );
                        consecutive_failures = 0;
                    }
                    let loaded = keys.len();
                    let before = admission_poll.len();
                    let revoked = admission_poll.replace_all(keys);
                    log_reload_diff("poll", loaded, before, &revoked);
                    if !revoked.is_empty() {
                        on_revoke_poll(revoked);
                    }
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::error!(
                        error = %e,
                        path = %poll_path,
                        consecutive_failures,
                        "remote-host: admission poll failed; serving the last successfully loaded allow-list"
                    );
                }
            }
        }
    });

    Ok(AdmissionDb {
        admission,
        pool,
        on_revoke,
    })
}

impl AdmissionDb {
    /// The live relay admission view.
    pub(crate) fn admission(&self) -> Arc<InMemoryAdmission> {
        self.admission.clone()
    }

    /// Reload the in-memory view from the DB (read-after-write, no poll-interval
    /// wait), firing `on_revoke` for any key the reload dropped — used right after a
    /// dashboard mutation so revokes take effect at once.
    pub(crate) async fn force_reload(&self) -> Result<(), AdmissionDbError> {
        let keys = load(&self.pool).await?;
        let loaded = keys.len();
        let before = self.admission.len();
        let revoked = self.admission.replace_all(keys);
        log_reload_diff("force_reload", loaded, before, &revoked);
        if !revoked.is_empty() {
            (self.on_revoke)(revoked);
        }
        Ok(())
    }

    /// Insert a new admitted key. Plain `INSERT` — a PK collision surfaces as
    /// [`AdmissionDbError::Db`] (the backend maps it to 409), never a silent upsert
    /// that would relax an existing key's limits. `expires_at` is honored.
    pub(crate) async fn admit_key(&self, new: &NewKey) -> Result<(), AdmissionDbError> {
        require_limits(new.max_conns, new.max_bps)?;
        let remote_api_key = new.remote_api_key.clone();
        let label = new.label.clone();
        let max_conns = to_i64(new.max_conns, "max_conns")?;
        let max_bps = to_i64(new.max_bps, "max_bps")?;
        let per_server_max_bps = to_i64(new.per_server_max_bps, "per_server_max_bps")?;
        let expires_at = new.expires_at.clone();
        self.pool
            .interact(move |conn| {
                conn.execute(
                    "INSERT INTO remote_api_keys \
                     (remote_api_key, label, max_conns, max_bps, per_server_max_bps, expires_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        remote_api_key,
                        label,
                        max_conns,
                        max_bps,
                        per_server_max_bps,
                        expires_at,
                    ],
                )
            })
            .await?;
        Ok(())
    }

    /// Update an existing key's full editable state (label + limits + expiry).
    /// `None` fields become SQL NULL. Returns [`AdmissionDbError::NotFound`] when no
    /// row matched. Lowering limits does not kick live legs — only a revoke does;
    /// the new limits bind on the next `register`/`limiter_for`.
    pub(crate) async fn edit_key(
        &self,
        remote_api_key: &str,
        limits: &KeyLimits,
    ) -> Result<(), AdmissionDbError> {
        require_limits(limits.max_conns, limits.max_bps)?;
        let remote_api_key = remote_api_key.to_owned();
        let label = limits.label.clone();
        let max_conns = to_i64(limits.max_conns, "max_conns")?;
        let max_bps = to_i64(limits.max_bps, "max_bps")?;
        let per_server_max_bps = to_i64(limits.per_server_max_bps, "per_server_max_bps")?;
        let expires_at = limits.expires_at.clone();
        let changed = self
            .pool
            .interact(move |conn| {
                conn.execute(
                    "UPDATE remote_api_keys \
                     SET label = ?2, max_conns = ?3, max_bps = ?4, \
                         per_server_max_bps = ?5, expires_at = ?6 \
                     WHERE remote_api_key = ?1",
                    params![
                        remote_api_key,
                        label,
                        max_conns,
                        max_bps,
                        per_server_max_bps,
                        expires_at,
                    ],
                )
            })
            .await?;
        if changed == 0 {
            return Err(AdmissionDbError::NotFound);
        }
        Ok(())
    }

    /// Delete an admitted key. Returns whether a row was removed (idempotent). The
    /// allow-list is infra, not session data, so this row-level delete is exempt
    /// from the never-delete-sessions rule.
    pub(crate) async fn revoke_key(&self, remote_api_key: &str) -> Result<bool, AdmissionDbError> {
        let remote_api_key = remote_api_key.to_owned();
        let deleted = self
            .pool
            .interact(move |conn| {
                conn.execute(
                    "DELETE FROM remote_api_keys WHERE remote_api_key = ?1",
                    params![remote_api_key],
                )
            })
            .await?;
        Ok(deleted > 0)
    }

    /// Every admitted row with its full columns, newest first. NO expiry filter —
    /// the operator must see expired rows. Carries the full secret; the API layer
    /// masks it to `key_last4` before serializing.
    pub(crate) async fn list_keys(&self) -> Result<Vec<KeyRecord>, AdmissionDbError> {
        let out = self
            .pool
            .interact(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT rowid, remote_api_key, label, max_conns, max_bps, \
                            per_server_max_bps, expires_at, created_at \
                     FROM remote_api_keys ORDER BY created_at DESC",
                )?;
                stmt.query_map([], |row| {
                    Ok(KeyRecord {
                        rowid: row.get::<_, i64>(0)?,
                        remote_api_key: row.get::<_, String>(1)?,
                        label: row.get::<_, Option<String>>(2)?,
                        max_conns: row
                            .get::<_, Option<i64>>(3)?
                            .and_then(|v| u32::try_from(v).ok()),
                        max_bps: row
                            .get::<_, Option<i64>>(4)?
                            .and_then(|v| u64::try_from(v).ok()),
                        per_server_max_bps: row
                            .get::<_, Option<i64>>(5)?
                            .and_then(|v| u64::try_from(v).ok()),
                        expires_at: row.get::<_, Option<String>>(6)?,
                        created_at: row.get::<_, String>(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        Ok(out)
    }

    /// The full secret for one row, addressed by its stable SQLite `rowid`. `None`
    /// when the row is gone. Masking is the API layer's job — this returns the key.
    pub(crate) async fn reveal_key(&self, rowid: i64) -> Result<Option<String>, AdmissionDbError> {
        let key = self
            .pool
            .interact(move |conn| {
                conn.query_row(
                    "SELECT remote_api_key FROM remote_api_keys WHERE rowid = ?1",
                    params![rowid],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .await?;
        Ok(key)
    }
}

/// A new key to admit. `expires_at` is the SQLite `datetime` wall-clock shape
/// (`"YYYY-MM-DD HH:MM:SS"` UTC).
pub(crate) struct NewKey {
    pub remote_api_key: String,
    pub label: Option<String>,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
}

/// The editable state of an existing key — `NewKey` minus the immutable PK.
pub(crate) struct KeyLimits {
    pub label: Option<String>,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
}

/// Every key must declare both `max_conns` and `max_bps` (mirrors the table
/// `CHECK`, but caught before the INSERT so the dashboard returns a clean 400).
fn require_limits(max_conns: Option<u32>, max_bps: Option<u64>) -> Result<(), AdmissionDbError> {
    if max_conns.is_none() || max_bps.is_none() {
        return Err(AdmissionDbError::MissingLimits);
    }
    Ok(())
}

/// Checked conversion of a limit column to sqlite's `i64` storage type — never an
/// `as` cast. `None` stays `None` (binds SQL NULL).
fn to_i64<T: TryInto<i64>>(
    v: Option<T>,
    field: &'static str,
) -> Result<Option<i64>, AdmissionDbError> {
    v.map(|x| {
        x.try_into()
            .map_err(|_| AdmissionDbError::LimitOutOfRange { field })
    })
    .transpose()
}

/// One row of the allow-list with its `rowid` and full secret, as read by
/// [`AdmissionDb::list_keys`].
pub(crate) struct KeyRecord {
    pub rowid: i64,
    pub remote_api_key: String,
    pub label: Option<String>,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// Generate a fresh `remote_api_key`: `"rh_"` + 32 random bytes hex-encoded.
pub(crate) fn generate_remote_api_key() -> String {
    format!(
        "{GENERATED_KEY_PREFIX}{}",
        hex::encode(rand::random::<[u8; GENERATED_KEY_BYTES]>())
    )
}

/// Log what one reload (poll tick or dashboard `force_reload`) changed; quiet
/// when the admitted set is unchanged. A key absent from the new set was deleted
/// out-of-band OR just crossed its `expires_at` (the load query filters expired
/// rows, so runtime expiry surfaces here as revocation). The revoked keys' live
/// connections are kicked by the caller's `on_revoke` hook right after this.
fn log_reload_diff(trigger: &str, keys_admitted: usize, before: usize, revoked: &HashSet<String>) {
    let added = (keys_admitted + revoked.len()).saturating_sub(before);
    if !revoked.is_empty() {
        let revoked_key_tags: Vec<String> = revoked.iter().map(|k| key_tag(k)).collect();
        tracing::info!(
            trigger,
            keys_admitted,
            added,
            revoked = revoked.len(),
            revoked_key_tags = ?revoked_key_tags,
            "remote-host: admission reload — keys lost admission (deleted or expired); kicking their live relay connections"
        );
    } else if added > 0 {
        tracing::info!(
            trigger,
            keys_admitted,
            added,
            "remote-host: admission reload — new keys admitted"
        );
    }
}

async fn load(pool: &SqlitePool) -> Result<HashMap<String, AdmissionEntry>, SqliteError> {
    pool.interact(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT remote_api_key, max_conns, max_bps, per_server_max_bps, expires_at \
             FROM remote_api_keys WHERE {NOT_EXPIRED}"
        ))?;
        let rows = stmt
            .query_map([], |row| {
                let key = row.get::<_, String>(0)?;
                // Nullable INTEGERs -> per-key overrides; NULL or out-of-range -> None
                // (the caller's role floor applies).
                let max_conns = row
                    .get::<_, Option<i64>>(1)?
                    .and_then(|v| u32::try_from(v).ok());
                let max_bps = row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|v| u64::try_from(v).ok());
                let per_server_max_bps = row
                    .get::<_, Option<i64>>(3)?
                    .and_then(|v| u64::try_from(v).ok());
                let expires_at = row.get::<_, Option<String>>(4)?;
                Ok((
                    key,
                    AdmissionEntry {
                        max_conns,
                        max_bps,
                        per_server_max_bps,
                        expires_at,
                    },
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::ops::Deref;

    /// A temp on-disk DB path that removes the `.db`/`-wal`/`-shm` sidecars on drop.
    struct TempDbPath(String);
    impl TempDbPath {
        fn new() -> Self {
            let p = std::env::temp_dir()
                .join(format!("admission-wal-{}.db", rand::random::<u64>()))
                .to_string_lossy()
                .into_owned();
            Self(p)
        }
    }
    impl Drop for TempDbPath {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0));
            }
        }
    }

    /// A pool over a throwaway on-disk DB, kept alive (and cleaned up) by the path
    /// guard it carries. Every pooled connection must see the same DB, so the tests
    /// use a real file rather than `:memory:` (which is private per connection).
    struct TempPool {
        pool: SqlitePool,
        _path: TempDbPath,
    }
    impl Deref for TempPool {
        type Target = SqlitePool;
        fn deref(&self) -> &SqlitePool {
            &self.pool
        }
    }

    /// An `AdmissionDb` over a throwaway on-disk DB, plus the path guard that cleans
    /// it up. The poller is not spawned — these tests drive `force_reload` explicitly,
    /// which is the read-after-write path.
    struct TempDb {
        db: AdmissionDb,
        _path: TempDbPath,
    }
    impl Deref for TempDb {
        type Target = AdmissionDb;
        fn deref(&self) -> &AdmissionDb {
            &self.db
        }
    }

    async fn temp_pool() -> TempPool {
        let path = TempDbPath::new();
        let pool = SqlitePool::open(&path.0).unwrap();
        pool.interact(|conn| conn.execute_batch(SCHEMA))
            .await
            .unwrap();
        TempPool { pool, _path: path }
    }

    /// Run one statement with no parameters, so a test can seed (or fail to seed) a
    /// row straight through the pool.
    async fn exec(pool: &SqlitePool, sql: &'static str) -> Result<usize, SqliteError> {
        pool.interact(move |conn| conn.execute(sql, [])).await
    }

    /// A no-op revoke hook for tests that don't assert on revocation.
    fn noop_hook() -> RevokeHook {
        Arc::new(|_revoked| {})
    }

    /// A recording revoke hook + the shared sink it pushes revoked keys into.
    fn recording_hook() -> (RevokeHook, Arc<Mutex<Vec<String>>>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let sink_for_hook = sink.clone();
        let hook: RevokeHook = Arc::new(move |revoked: HashSet<String>| {
            let mut got: Vec<String> = revoked.into_iter().collect();
            got.sort();
            sink_for_hook.lock().extend(got);
        });
        (hook, sink)
    }

    async fn temp_db(on_revoke: RevokeHook) -> TempDb {
        let TempPool { pool, _path } = temp_pool().await;
        let admission = Arc::new(InMemoryAdmission::new());
        let _ = admission.replace_all(load(&pool).await.unwrap());
        TempDb {
            db: AdmissionDb {
                admission,
                pool,
                on_revoke,
            },
            _path,
        }
    }

    fn new_key(key: &str, max_conns: Option<u32>, max_bps: Option<u64>) -> NewKey {
        NewKey {
            remote_api_key: key.to_string(),
            label: None,
            max_conns,
            max_bps,
            per_server_max_bps: None,
            expires_at: None,
        }
    }

    /// `load` reads every limit column and `expires_at`.
    #[tokio::test]
    async fn load_reads_every_limit_column() {
        let pool = temp_pool().await;
        exec(
            &pool,
            "INSERT INTO remote_api_keys(remote_api_key, max_conns, max_bps, per_server_max_bps) \
             VALUES('tuned', 8, 4194304, 1048576)",
        )
        .await
        .unwrap();

        let loaded = load(&pool).await.unwrap();

        let tuned = loaded.get("tuned").expect("row admitted");
        assert_eq!(tuned.max_conns, Some(8));
        assert_eq!(tuned.max_bps, Some(4_194_304));
        assert_eq!(tuned.per_server_max_bps, Some(1_048_576));
        assert_eq!(tuned.expires_at, None);
    }

    /// An expired row is filtered out of the in-memory view; a row with no expiry
    /// and a far-future one both survive.
    #[tokio::test]
    async fn load_filters_expired() {
        let pool = temp_pool().await;
        exec(
            &pool,
            "INSERT INTO remote_api_keys(remote_api_key, max_conns, max_bps, expires_at) \
             VALUES('stale', 8, 4194304, '2000-01-01 00:00:00')",
        )
        .await
        .unwrap();
        exec(
            &pool,
            "INSERT INTO remote_api_keys(remote_api_key, max_conns, max_bps, expires_at) \
             VALUES('fresh', 8, 4194304, '2999-01-01 00:00:00')",
        )
        .await
        .unwrap();
        exec(
            &pool,
            "INSERT INTO remote_api_keys(remote_api_key, max_conns, max_bps) \
             VALUES('never', 8, 4194304)",
        )
        .await
        .unwrap();

        let loaded = load(&pool).await.unwrap();
        assert!(!loaded.contains_key("stale"), "expired row is filtered");
        assert!(loaded.contains_key("fresh"), "future expiry kept");
        assert!(loaded.contains_key("never"), "no-expiry row kept");
    }

    /// The `CHECK` requires `max_conns` + `max_bps` on every row;
    /// `per_server_max_bps` is never required.
    #[tokio::test]
    async fn row_requires_max_conns_and_max_bps() {
        let pool = temp_pool().await;

        // Both limits set + per_server NULL → accepted.
        exec(
            &pool,
            "INSERT INTO remote_api_keys(remote_api_key, max_conns, max_bps) \
             VALUES('ok', 8, 4194304)",
        )
        .await
        .unwrap();

        // Missing max_bps → rejected.
        let missing_bps = exec(
            &pool,
            "INSERT INTO remote_api_keys(remote_api_key, max_conns) VALUES('no-bps', 8)",
        )
        .await;
        assert!(missing_bps.is_err(), "a row must set max_bps");

        // Missing both (the old bare admit) → rejected.
        let bare = exec(
            &pool,
            "INSERT INTO remote_api_keys(remote_api_key, label) VALUES('bare', 'gw')",
        )
        .await;
        assert!(bare.is_err(), "a row can't omit both limits");
    }

    /// An admit with both limits lands and surfaces in `list_keys`.
    #[tokio::test]
    async fn admit_with_both_limits_ok() {
        let db = temp_db(noop_hook()).await;
        db.admit_key(&new_key("reg", Some(8), Some(4_194_304)))
            .await
            .unwrap();
        let keys = db.list_keys().await.unwrap();
        let row = keys.iter().find(|k| k.remote_api_key == "reg").unwrap();
        assert_eq!(row.max_conns, Some(8));
        assert_eq!(row.max_bps, Some(4_194_304));
    }

    /// An admit with a NULL limit is rejected before the INSERT.
    #[tokio::test]
    async fn admit_missing_limit_is_rejected() {
        let db = temp_db(noop_hook()).await;
        let err = db
            .admit_key(&new_key("reg", Some(8), None))
            .await
            .unwrap_err();
        assert!(matches!(err, AdmissionDbError::MissingLimits));
        assert!(db.list_keys().await.unwrap().is_empty());
    }

    /// An admit with a label + expiry round-trips through `list_keys`.
    #[tokio::test]
    async fn admit_with_label_and_expiry_ok() {
        let db = temp_db(noop_hook()).await;
        db.admit_key(&NewKey {
            remote_api_key: "g".into(),
            label: Some("trial".into()),
            max_conns: Some(8),
            max_bps: Some(4_194_304),
            per_server_max_bps: None,
            expires_at: Some("2999-01-01 00:00:00".into()),
        })
        .await
        .unwrap();
        let keys = db.list_keys().await.unwrap();
        let row = keys.iter().find(|k| k.remote_api_key == "g").unwrap();
        assert_eq!(row.label.as_deref(), Some("trial"));
        assert_eq!(row.expires_at.as_deref(), Some("2999-01-01 00:00:00"));
    }

    /// A duplicate admit fails (no silent upsert that would relax limits).
    #[tokio::test]
    async fn duplicate_admit_is_a_db_error() {
        let db = temp_db(noop_hook()).await;
        db.admit_key(&new_key("dup", Some(4), Some(1024)))
            .await
            .unwrap();
        let err = db
            .admit_key(&new_key("dup", Some(99), Some(9_999)))
            .await
            .unwrap_err();
        assert!(matches!(err, AdmissionDbError::Db(_)));
        // The original limits are untouched.
        let row = db
            .list_keys()
            .await
            .unwrap()
            .into_iter()
            .find(|k| k.remote_api_key == "dup")
            .unwrap();
        assert_eq!(row.max_conns, Some(4));
    }

    /// `edit_key` on a missing key → `NotFound`; on an existing key the new state is
    /// reflected by `list_keys`.
    #[tokio::test]
    async fn edit_key_missing_and_existing() {
        let db = temp_db(noop_hook()).await;
        let missing = db
            .edit_key(
                "nope",
                &KeyLimits {
                    label: None,
                    max_conns: Some(1),
                    max_bps: Some(1),
                    per_server_max_bps: None,
                    expires_at: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, AdmissionDbError::NotFound));

        db.admit_key(&new_key("e", Some(2), Some(2048)))
            .await
            .unwrap();
        db.edit_key(
            "e",
            &KeyLimits {
                label: Some("renamed".into()),
                max_conns: Some(16),
                max_bps: Some(8_388_608),
                per_server_max_bps: Some(1_048_576),
                expires_at: None,
            },
        )
        .await
        .unwrap();
        let row = db
            .list_keys()
            .await
            .unwrap()
            .into_iter()
            .find(|k| k.remote_api_key == "e")
            .unwrap();
        assert_eq!(row.label.as_deref(), Some("renamed"));
        assert_eq!(row.max_conns, Some(16));
        assert_eq!(row.max_bps, Some(8_388_608));
        assert_eq!(row.per_server_max_bps, Some(1_048_576));
    }

    /// `revoke_key` is idempotent: first delete returns `true`, the second `false`.
    #[tokio::test]
    async fn revoke_key_is_idempotent() {
        let db = temp_db(noop_hook()).await;
        db.admit_key(&new_key("r", Some(1), Some(1))).await.unwrap();
        assert!(db.revoke_key("r").await.unwrap(), "first delete removes it");
        assert!(!db.revoke_key("r").await.unwrap(), "second is a no-op");
    }

    /// After a revoke, `force_reload` diffs the in-memory view against the table and
    /// fires the hook with exactly the revoked set.
    #[tokio::test]
    async fn force_reload_after_revoke_fires_hook() {
        let (hook, sink) = recording_hook();
        let db = temp_db(hook).await;
        db.admit_key(&new_key("k1", Some(1), Some(1)))
            .await
            .unwrap();
        db.admit_key(&new_key("k2", Some(1), Some(1)))
            .await
            .unwrap();
        // Seed the in-memory view so the next reload can diff a removal.
        db.force_reload().await.unwrap();
        assert!(sink.lock().is_empty(), "no revokes on the seeding reload");

        assert!(db.revoke_key("k1").await.unwrap());
        db.force_reload().await.unwrap();
        assert_eq!(*sink.lock(), vec!["k1".to_string()]);
    }

    /// End-to-end revoke → kick: wire `on_revoke` to a real `ConnectionRegistry` +
    /// `BandwidthRegistry` (exactly the closure `run()` builds), register a live
    /// connection under `k1`, then revoke + `force_reload`. The diff yields `k1`,
    /// the hook fires `conns.kick` synchronously, and the connection's kick channel
    /// resolves.
    #[tokio::test]
    async fn force_reload_revoke_kicks_a_live_connection() {
        use remote_host_relay::{BandwidthRegistry, ConnectionRegistry};

        let conns = Arc::new(ConnectionRegistry::new());
        let bandwidth = Arc::new(BandwidthRegistry::new());
        let hook: RevokeHook = {
            let conns = conns.clone();
            let bandwidth = bandwidth.clone();
            Arc::new(move |revoked: HashSet<String>| {
                conns.kick(&revoked);
                bandwidth.forget(&revoked);
            })
        };

        let db = temp_db(hook).await;
        db.admit_key(&new_key("k1", Some(1), Some(1)))
            .await
            .unwrap();
        // Seed the in-memory view so the post-revoke reload can diff the removal.
        db.force_reload().await.unwrap();

        // A live connection under k1, holding the kick channel.
        let (_guard, rx) = conns.register_for_test("k1");
        assert_eq!(conns.live_for("k1"), 1);

        assert!(db.revoke_key("k1").await.unwrap());
        db.force_reload().await.unwrap();

        assert_eq!(conns.live_for("k1"), 0, "kick dropped the live connection");
        assert!(
            rx.await.is_err(),
            "the kick channel resolved (sender dropped) synchronously"
        );
    }

    /// `reveal_key` round-trips by `rowid`; an unknown rowid is `Ok(None)`.
    #[tokio::test]
    async fn reveal_key_round_trips() {
        let db = temp_db(noop_hook()).await;
        db.admit_key(&new_key("secret-key", Some(1), Some(1)))
            .await
            .unwrap();
        let rowid = db
            .list_keys()
            .await
            .unwrap()
            .into_iter()
            .find(|k| k.remote_api_key == "secret-key")
            .unwrap()
            .rowid;
        assert_eq!(
            db.reveal_key(rowid).await.unwrap().as_deref(),
            Some("secret-key")
        );
        assert_eq!(db.reveal_key(999_999).await.unwrap(), None);
    }

    /// `list_keys` carries `label` + `created_at` (non-empty `datetime` default).
    #[tokio::test]
    async fn list_keys_includes_label_and_created_at() {
        let db = temp_db(noop_hook()).await;
        db.admit_key(&NewKey {
            remote_api_key: "labeled".into(),
            label: Some("gw-1".into()),
            max_conns: Some(1),
            max_bps: Some(1),
            per_server_max_bps: None,
            expires_at: None,
        })
        .await
        .unwrap();
        let row = db
            .list_keys()
            .await
            .unwrap()
            .into_iter()
            .find(|k| k.remote_api_key == "labeled")
            .unwrap();
        assert_eq!(row.label.as_deref(), Some("gw-1"));
        assert!(!row.created_at.is_empty(), "created_at defaulted by SQLite");
    }

    /// A generated key has the `rh_` prefix + 64 hex chars (32 bytes) and is unique.
    #[test]
    fn generated_key_shape_and_uniqueness() {
        let a = generate_remote_api_key();
        let b = generate_remote_api_key();
        assert!(a.starts_with(GENERATED_KEY_PREFIX));
        assert_eq!(
            a.len(),
            GENERATED_KEY_PREFIX.len() + GENERATED_KEY_BYTES * 2
        );
        assert!(
            a[GENERATED_KEY_PREFIX.len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
        assert_ne!(a, b);
    }

    /// WAL coexistence on a real on-disk file: two independent pools on the same
    /// path, both WAL; a write on one is visible to the other's `load` after commit.
    #[tokio::test]
    async fn wal_writer_and_reader_coexist() {
        let path = TempDbPath::new();
        let db = open(&path.0, Duration::from_secs(3600), noop_hook())
            .await
            .unwrap();

        // A second, independent reader pool on the same on-disk file.
        let reader = SqlitePool::open(&path.0).unwrap();

        db.admit_key(&new_key("wal-key", Some(2), Some(2048)))
            .await
            .unwrap();

        let seen = load(&reader).await.unwrap();
        assert!(
            seen.contains_key("wal-key"),
            "reader sees the committed write across connections"
        );
    }
}
