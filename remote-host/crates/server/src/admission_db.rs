//! The admission allow-list, sourced from a SQLite (libsql) table and
//! hot-reloaded by polling — the table may be updated out of band (another
//! process writes it), so the runtime re-reads it on an interval rather than
//! reacting to in-process writes.
//!
//! [`AdmissionDb`] also owns the in-process **write path** the operator dashboard
//! drives (admit / edit / revoke / reveal / list). Writes go through the same
//! `write_conn` whose subsequent [`AdmissionDb::force_reload`] reloads the
//! in-memory view read-after-write on that one connection, firing the shared
//! `on_revoke` hook for any key that just lost admission.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use libsql::{Builder, params};
use remote_host_admission::{AdmissionEntry, InMemoryAdmission, Tier};

/// Source-of-truth table: one row per admitted `remote_api_key`. `label` +
/// `created_at` are for whoever administers it. `tier` is `'guest'` (auto-issued,
/// carries the guest default limits, GC-eligible) or `'registered'` (control-plane
/// provisioned, explicit per-row limits). `max_conns` / `max_bps` are **required on
/// a registered row** (a registered key must declare its own limits) but stay
/// optional on a guest row, which inherits a NULL column from the `'guest'` template
/// row (else the `GUEST_*` const) — enforced by the `CHECK`. `per_server_max_bps` is
/// always optional (NULL → falls back to the row's `max_bps`). `expires_at` is the
/// guest-TTL wall clock (NULL → never expires).
///
/// The `CHECK` only guards a freshly-created table; `CREATE TABLE IF NOT EXISTS`
/// can't retrofit it onto a DB made under an older schema.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS remote_api_keys (\
    remote_api_key TEXT PRIMARY KEY, \
    label TEXT, \
    tier TEXT NOT NULL DEFAULT 'registered', \
    max_conns INTEGER, \
    max_bps INTEGER, \
    per_server_max_bps INTEGER, \
    expires_at TEXT, \
    created_at TEXT NOT NULL DEFAULT (datetime('now')), \
    CHECK (tier = 'guest' OR (max_conns IS NOT NULL AND max_bps IS NOT NULL)))";

/// Keep an admitted-but-expired guest out of the in-memory view: drop a row only
/// when it is a guest, carries an `expires_at`, and that instant has passed. NULL
/// expiry and registered rows are always kept.
const NOT_EXPIRED_GUEST: &str =
    "NOT (tier = 'guest' AND expires_at IS NOT NULL AND expires_at < datetime('now'))";

/// Generated-key shape (per the dashboard contract): `"rh_" + hex(32 random bytes)`.
const GENERATED_KEY_BYTES: usize = 32;
const GENERATED_KEY_PREFIX: &str = "rh_";

/// Errors raised by the admission write/read path. Reads/writes flow through one
/// libsql connection; a PK collision on admit surfaces here as [`Self::Db`] and is
/// mapped to a 409 by the dashboard backend.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionDbError {
    #[error("admission db: {0}")]
    Db(#[from] libsql::Error),
    #[error("remote_api_key not found")]
    NotFound,
    #[error("a registered key requires both max_conns and max_bps")]
    MissingRegisteredLimits,
    #[error("limit {field} value out of range")]
    LimitOutOfRange { field: &'static str },
}

/// Called with the keys that just lost admission on every reload (poll tick or
/// [`AdmissionDb::force_reload`]) so their live connections + bandwidth buckets can
/// be dropped. Shared by the poller task and the write path's reload, hence `Arc`.
pub(crate) type RevokeHook = Arc<dyn Fn(HashSet<String>) + Send + Sync>;

/// The admission allow-list handle: the live in-memory view plus the write
/// connection the dashboard mutates. The poller task (spawned by [`open`]) holds the
/// owning [`libsql::Database`] alive, which is what keeps `write_conn` valid.
pub(crate) struct AdmissionDb {
    admission: Arc<InMemoryAdmission>,
    write_conn: libsql::Connection,
    on_revoke: RevokeHook,
}

/// Open the libsql DB at `path` in WAL mode, ensure the table, load the allow-list,
/// and spawn a task that re-reads it every `poll` to pick up external edits (via a
/// **separate** reader connection — the WAL win). On each reload, `on_revoke` is
/// called with the keys that just lost admission so their live connections can be
/// dropped. Returns a handle owning the write connection + shared admission view.
pub(crate) async fn open(
    path: &str,
    poll: Duration,
    on_revoke: RevokeHook,
) -> Result<AdmissionDb, AdmissionDbError> {
    let db = Builder::new_local(path).build().await?;
    let write_conn = db.connect()?;
    // `PRAGMA journal_mode=WAL` returns a row → run it via `query` + drain, not
    // `execute`. WAL lets the poller's reader connection read concurrently with
    // dashboard writes on `write_conn`.
    let mut r = write_conn.query("PRAGMA journal_mode=WAL", ()).await?;
    let _ = r.next().await?;
    // `PRAGMA busy_timeout=N` echoes the new value as a row in this libsql build —
    // drain it via `query`, same as `journal_mode`, or `execute` errors.
    let mut r = write_conn.query("PRAGMA busy_timeout=5000", ()).await?;
    let _ = r.next().await?;
    write_conn.execute(SCHEMA, ()).await?;

    let admission = Arc::new(InMemoryAdmission::new());
    // Initial load: nothing was admitted before, so nothing to revoke.
    let _ = admission.replace_all(load(&write_conn).await?);

    let read_conn = db.connect()?;
    let admission_poll = admission.clone();
    let on_revoke_poll = on_revoke.clone();
    tokio::spawn(async move {
        // Hold `_db` for the task's life: it keeps BOTH the moved `read_conn` here
        // AND the `write_conn` returned to the caller valid (a libsql `Connection`
        // outliving its `Database` is unsound). The poller never exits, so the
        // write connection stays usable for the dashboard's whole runtime.
        let _db = db;
        let mut tick = tokio::time::interval(poll);
        tick.tick().await; // the first tick is immediate; we already loaded once
        loop {
            tick.tick().await;
            match load(&read_conn).await {
                Ok(keys) => {
                    let revoked = admission_poll.replace_all(keys);
                    if !revoked.is_empty() {
                        on_revoke_poll(revoked);
                    }
                }
                Err(e) => tracing::error!(error = %e, "remote-host: admission poll failed"),
            }
        }
    });

    Ok(AdmissionDb {
        admission,
        write_conn,
        on_revoke,
    })
}

impl AdmissionDb {
    /// The live admission view shared by both roles (relay + push resolve against it).
    pub(crate) fn admission(&self) -> Arc<InMemoryAdmission> {
        self.admission.clone()
    }

    /// Reload the in-memory view from the write connection (read-after-write on one
    /// connection, no poll-interval wait), firing `on_revoke` for any key the reload
    /// dropped — used right after a dashboard mutation so revokes take effect at once.
    pub(crate) async fn force_reload(&self) -> Result<(), AdmissionDbError> {
        let revoked = self.admission.replace_all(load(&self.write_conn).await?);
        if !revoked.is_empty() {
            (self.on_revoke)(revoked);
        }
        Ok(())
    }

    /// Insert a new admitted key. Plain `INSERT` — a PK collision surfaces as
    /// [`AdmissionDbError::Db`] (the backend maps it to 409), never a silent upsert
    /// that would relax an existing key's limits. `tier` + `expires_at` are honored.
    pub(crate) async fn admit_key(&self, new: &NewKey) -> Result<(), AdmissionDbError> {
        require_registered_limits(new.tier, new.max_conns, new.max_bps)?;
        self.write_conn
            .execute(
                "INSERT INTO remote_api_keys \
                 (remote_api_key, label, tier, max_conns, max_bps, per_server_max_bps, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    new.remote_api_key.clone(),
                    new.label.clone(),
                    new.tier.as_str(),
                    to_i64(new.max_conns, "max_conns")?,
                    to_i64(new.max_bps, "max_bps")?,
                    to_i64(new.per_server_max_bps, "per_server_max_bps")?,
                    new.expires_at.clone(),
                ],
            )
            .await?;
        Ok(())
    }

    /// Update an existing key's full editable state (label + tier + limits + expiry).
    /// `None` fields become SQL NULL. Returns [`AdmissionDbError::NotFound`] when no
    /// row matched. Lowering limits does not kick live legs — only a revoke does;
    /// the new limits bind on the next `register`/`limiter_for`.
    pub(crate) async fn edit_key(
        &self,
        remote_api_key: &str,
        limits: &KeyLimits,
    ) -> Result<(), AdmissionDbError> {
        require_registered_limits(limits.tier, limits.max_conns, limits.max_bps)?;
        let changed = self
            .write_conn
            .execute(
                "UPDATE remote_api_keys \
                 SET label = ?2, tier = ?3, max_conns = ?4, max_bps = ?5, \
                     per_server_max_bps = ?6, expires_at = ?7 \
                 WHERE remote_api_key = ?1",
                params![
                    remote_api_key,
                    limits.label.clone(),
                    limits.tier.as_str(),
                    to_i64(limits.max_conns, "max_conns")?,
                    to_i64(limits.max_bps, "max_bps")?,
                    to_i64(limits.per_server_max_bps, "per_server_max_bps")?,
                    limits.expires_at.clone(),
                ],
            )
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
        let deleted = self
            .write_conn
            .execute(
                "DELETE FROM remote_api_keys WHERE remote_api_key = ?1",
                params![remote_api_key],
            )
            .await?;
        Ok(deleted > 0)
    }

    /// Every admitted row with its full columns, newest first. NO expired-guest
    /// filter — the operator must see expired/guest rows. Carries the full secret;
    /// the API layer masks it to `key_last4` before serializing.
    pub(crate) async fn list_keys(&self) -> Result<Vec<KeyRecord>, AdmissionDbError> {
        let mut rows = self
            .write_conn
            .query(
                "SELECT rowid, remote_api_key, label, tier, max_conns, max_bps, \
                        per_server_max_bps, expires_at, created_at \
                 FROM remote_api_keys ORDER BY created_at DESC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(KeyRecord {
                rowid: row.get::<i64>(0)?,
                remote_api_key: row.get::<String>(1)?,
                label: row.get::<Option<String>>(2)?,
                tier: Tier::from_str(&row.get::<String>(3)?).unwrap_or_default(),
                max_conns: row
                    .get::<Option<i64>>(4)?
                    .and_then(|v| u32::try_from(v).ok()),
                max_bps: row
                    .get::<Option<i64>>(5)?
                    .and_then(|v| u64::try_from(v).ok()),
                per_server_max_bps: row
                    .get::<Option<i64>>(6)?
                    .and_then(|v| u64::try_from(v).ok()),
                expires_at: row.get::<Option<String>>(7)?,
                created_at: row.get::<String>(8)?,
            });
        }
        Ok(out)
    }

    /// The full secret for one row, addressed by its stable SQLite `rowid`. `None`
    /// when the row is gone. Masking is the API layer's job — this returns the key.
    pub(crate) async fn reveal_key(&self, rowid: i64) -> Result<Option<String>, AdmissionDbError> {
        let mut rows = self
            .write_conn
            .query(
                "SELECT remote_api_key FROM remote_api_keys WHERE rowid = ?1",
                params![rowid],
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row.get::<String>(0)?)),
            None => Ok(None),
        }
    }
}

/// A new key to admit. `expires_at` is the SQLite `datetime` wall-clock shape
/// (`"YYYY-MM-DD HH:MM:SS"` UTC).
pub(crate) struct NewKey {
    pub remote_api_key: String,
    pub label: Option<String>,
    pub tier: Tier,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
}

/// The editable state of an existing key — `NewKey` minus the immutable PK.
pub(crate) struct KeyLimits {
    pub label: Option<String>,
    pub tier: Tier,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
}

/// A registered key must declare both `max_conns` and `max_bps` (mirrors the table
/// `CHECK`, but caught before the INSERT so the dashboard returns a clean 400).
fn require_registered_limits(
    tier: Tier,
    max_conns: Option<u32>,
    max_bps: Option<u64>,
) -> Result<(), AdmissionDbError> {
    if tier == Tier::Registered && (max_conns.is_none() || max_bps.is_none()) {
        return Err(AdmissionDbError::MissingRegisteredLimits);
    }
    Ok(())
}

/// Checked conversion of a limit column to libsql's `i64` storage type — never an
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
    pub tier: Tier,
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

async fn load(conn: &libsql::Connection) -> Result<HashMap<String, AdmissionEntry>, libsql::Error> {
    let mut rows = conn
        .query(
            &format!(
                "SELECT remote_api_key, tier, max_conns, max_bps, per_server_max_bps, expires_at \
                 FROM remote_api_keys WHERE {NOT_EXPIRED_GUEST}"
            ),
            (),
        )
        .await?;
    let mut keys = HashMap::new();
    while let Some(row) = rows.next().await? {
        let key = row.get::<String>(0)?;
        // `tier` is NOT NULL DEFAULT 'registered'; an unknown string falls back to
        // the conservative registered tier (caller floors NULL limits).
        let tier = Tier::from_str(&row.get::<String>(1)?).unwrap_or_default();
        // Nullable INTEGERs -> per-key overrides; NULL or out-of-range -> None
        // (the guest default or the caller's floor applies).
        let max_conns = row
            .get::<Option<i64>>(2)?
            .and_then(|v| u32::try_from(v).ok());
        let max_bps = row
            .get::<Option<i64>>(3)?
            .and_then(|v| u64::try_from(v).ok());
        let per_server_max_bps = row
            .get::<Option<i64>>(4)?
            .and_then(|v| u64::try_from(v).ok());
        let expires_at = row.get::<Option<String>>(5)?;
        keys.insert(
            key,
            AdmissionEntry {
                tier,
                max_conns,
                max_bps,
                per_server_max_bps,
                expires_at,
            },
        );
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    async fn mem_conn() -> libsql::Connection {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(SCHEMA, ()).await.unwrap();
        conn
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

    /// Build an `AdmissionDb` over an in-memory libsql DB with the given revoke
    /// hook. The poller is not spawned (no `path`/`Database` to keep alive) — these
    /// tests drive `force_reload` explicitly, which is the read-after-write path.
    async fn mem_db(on_revoke: RevokeHook) -> AdmissionDb {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let write_conn = db.connect().unwrap();
        write_conn.execute(SCHEMA, ()).await.unwrap();
        let admission = Arc::new(InMemoryAdmission::new());
        let _ = admission.replace_all(load(&write_conn).await.unwrap());
        // Keep the in-memory `Database` alive for the test's duration by leaking it
        // (test-only): an in-memory libsql connection is invalid once its `Database`
        // drops, and there is no poller task here to hold it.
        Box::leak(Box::new(db));
        AdmissionDb {
            admission,
            write_conn,
            on_revoke,
        }
    }

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

    fn registered(key: &str, max_conns: Option<u32>, max_bps: Option<u64>) -> NewKey {
        NewKey {
            remote_api_key: key.to_string(),
            label: None,
            tier: Tier::Registered,
            max_conns,
            max_bps,
            per_server_max_bps: None,
            expires_at: None,
        }
    }

    /// `load` reads every column: tier, all three limits, and `expires_at`.
    #[tokio::test]
    async fn load_reads_tier_and_every_limit_column() {
        let conn = mem_conn().await;
        conn.execute(
            "INSERT INTO remote_api_keys(remote_api_key, tier, max_conns, max_bps, per_server_max_bps) \
             VALUES('tuned', 'registered', 8, 4194304, 1048576)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO remote_api_keys(remote_api_key, tier) VALUES('bare-guest', 'guest')",
            (),
        )
        .await
        .unwrap();

        let loaded = load(&conn).await.unwrap();

        let tuned = loaded.get("tuned").expect("registered row admitted");
        assert_eq!(tuned.tier, Tier::Registered);
        assert_eq!(tuned.max_conns, Some(8));
        assert_eq!(tuned.max_bps, Some(4_194_304));
        assert_eq!(tuned.per_server_max_bps, Some(1_048_576));
        assert_eq!(tuned.expires_at, None);

        let guest = loaded.get("bare-guest").expect("guest row admitted");
        assert_eq!(guest.tier, Tier::Guest);
        assert_eq!(
            guest.max_conns, None,
            "NULL stays None; resolve() defaults it"
        );
    }

    /// An expired guest row is filtered out of the in-memory view; a registered
    /// row past its (unusual) `expires_at` and a far-future guest both survive.
    #[tokio::test]
    async fn load_filters_only_expired_guests() {
        let conn = mem_conn().await;
        conn.execute(
            "INSERT INTO remote_api_keys(remote_api_key, tier, expires_at) \
             VALUES('stale', 'guest', '2000-01-01 00:00:00')",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO remote_api_keys(remote_api_key, tier, expires_at) \
             VALUES('fresh', 'guest', '2999-01-01 00:00:00')",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO remote_api_keys(remote_api_key, tier, max_conns, max_bps, expires_at) \
             VALUES('reg-stale', 'registered', 8, 4194304, '2000-01-01 00:00:00')",
            (),
        )
        .await
        .unwrap();

        let loaded = load(&conn).await.unwrap();
        assert!(!loaded.contains_key("stale"), "expired guest is filtered");
        assert!(loaded.contains_key("fresh"), "future guest kept");
        assert!(
            loaded.contains_key("reg-stale"),
            "the load filter only drops guests"
        );
    }

    /// The `CHECK` requires `max_conns` + `max_bps` on a registered row but exempts
    /// guests; `per_server_max_bps` is never required.
    #[tokio::test]
    async fn registered_row_requires_max_conns_and_max_bps() {
        let conn = mem_conn().await;

        // Registered, both limits set + per_server NULL → accepted.
        conn.execute(
            "INSERT INTO remote_api_keys(remote_api_key, tier, max_conns, max_bps) \
             VALUES('ok', 'registered', 8, 4194304)",
            (),
        )
        .await
        .unwrap();

        // Registered missing max_bps → rejected.
        let missing_bps = conn
            .execute(
                "INSERT INTO remote_api_keys(remote_api_key, tier, max_conns) \
                 VALUES('no-bps', 'registered', 8)",
                (),
            )
            .await;
        assert!(missing_bps.is_err(), "registered must set max_bps");

        // Registered missing both (the old bare admit) → rejected.
        let bare = conn
            .execute(
                "INSERT INTO remote_api_keys(remote_api_key, label) VALUES('bare', 'gw')",
                (),
            )
            .await;
        assert!(bare.is_err(), "a registered row can't omit both limits");

        // Guest with no limits → accepted (it inherits the template / consts).
        conn.execute(
            "INSERT INTO remote_api_keys(remote_api_key, tier) VALUES('g', 'guest')",
            (),
        )
        .await
        .unwrap();
    }

    /// A registered admit with both limits lands and surfaces in `list_keys`.
    #[tokio::test]
    async fn registered_admit_with_both_limits_ok() {
        let db = mem_db(noop_hook()).await;
        db.admit_key(&registered("reg", Some(8), Some(4_194_304)))
            .await
            .unwrap();
        let keys = db.list_keys().await.unwrap();
        let row = keys.iter().find(|k| k.remote_api_key == "reg").unwrap();
        assert_eq!(row.tier, Tier::Registered);
        assert_eq!(row.max_conns, Some(8));
        assert_eq!(row.max_bps, Some(4_194_304));
    }

    /// A registered admit with a NULL limit is rejected before the INSERT.
    #[tokio::test]
    async fn registered_admit_missing_limit_is_rejected() {
        let db = mem_db(noop_hook()).await;
        let err = db
            .admit_key(&registered("reg", Some(8), None))
            .await
            .unwrap_err();
        assert!(matches!(err, AdmissionDbError::MissingRegisteredLimits));
        assert!(db.list_keys().await.unwrap().is_empty());
    }

    /// A guest admit with NULL limits and a TTL is accepted.
    #[tokio::test]
    async fn guest_admit_with_null_limits_ok() {
        let db = mem_db(noop_hook()).await;
        db.admit_key(&NewKey {
            remote_api_key: "g".into(),
            label: Some("trial".into()),
            tier: Tier::Guest,
            max_conns: None,
            max_bps: None,
            per_server_max_bps: None,
            expires_at: Some("2999-01-01 00:00:00".into()),
        })
        .await
        .unwrap();
        let keys = db.list_keys().await.unwrap();
        let row = keys.iter().find(|k| k.remote_api_key == "g").unwrap();
        assert_eq!(row.tier, Tier::Guest);
        assert_eq!(row.label.as_deref(), Some("trial"));
        assert_eq!(row.expires_at.as_deref(), Some("2999-01-01 00:00:00"));
    }

    /// A duplicate admit fails (no silent upsert that would relax limits).
    #[tokio::test]
    async fn duplicate_admit_is_a_db_error() {
        let db = mem_db(noop_hook()).await;
        db.admit_key(&registered("dup", Some(4), Some(1024)))
            .await
            .unwrap();
        let err = db
            .admit_key(&registered("dup", Some(99), Some(9_999)))
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
        let db = mem_db(noop_hook()).await;
        let missing = db
            .edit_key(
                "nope",
                &KeyLimits {
                    label: None,
                    tier: Tier::Registered,
                    max_conns: Some(1),
                    max_bps: Some(1),
                    per_server_max_bps: None,
                    expires_at: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, AdmissionDbError::NotFound));

        db.admit_key(&registered("e", Some(2), Some(2048)))
            .await
            .unwrap();
        db.edit_key(
            "e",
            &KeyLimits {
                label: Some("renamed".into()),
                tier: Tier::Registered,
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
        let db = mem_db(noop_hook()).await;
        db.admit_key(&registered("r", Some(1), Some(1)))
            .await
            .unwrap();
        assert!(db.revoke_key("r").await.unwrap(), "first delete removes it");
        assert!(!db.revoke_key("r").await.unwrap(), "second is a no-op");
    }

    /// After a revoke, `force_reload` diffs the in-memory view against the table and
    /// fires the hook with exactly the revoked set.
    #[tokio::test]
    async fn force_reload_after_revoke_fires_hook() {
        let (hook, sink) = recording_hook();
        let db = mem_db(hook).await;
        db.admit_key(&registered("k1", Some(1), Some(1)))
            .await
            .unwrap();
        db.admit_key(&registered("k2", Some(1), Some(1)))
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

        let db = mem_db(hook).await;
        db.admit_key(&registered("k1", Some(1), Some(1)))
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
        let db = mem_db(noop_hook()).await;
        db.admit_key(&registered("secret-key", Some(1), Some(1)))
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
        let db = mem_db(noop_hook()).await;
        db.admit_key(&NewKey {
            remote_api_key: "labeled".into(),
            label: Some("gw-1".into()),
            tier: Tier::Registered,
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

    /// WAL coexistence on a real on-disk file: two `Database`s on the same path, both
    /// WAL; a write on one is visible to the other's `load` after commit.
    #[tokio::test]
    async fn wal_writer_and_reader_coexist() {
        let path = TempDbPath::new();
        let db = open(&path.0, Duration::from_secs(3600), noop_hook())
            .await
            .unwrap();

        // A second, independent reader connection on the same on-disk file.
        let reader_db = Builder::new_local(&path.0).build().await.unwrap();
        let reader = reader_db.connect().unwrap();
        let mut r = reader.query("PRAGMA journal_mode=WAL", ()).await.unwrap();
        let _ = r.next().await.unwrap();

        db.admit_key(&registered("wal-key", Some(2), Some(2048)))
            .await
            .unwrap();

        let seen = load(&reader).await.unwrap();
        assert!(
            seen.contains_key("wal-key"),
            "reader sees the committed write across connections"
        );
    }
}
