//! The admission allow-list, sourced from a SQLite (libsql) table and
//! hot-reloaded by polling — the table may be updated out of band (another
//! process writes it), so the runtime re-reads it on an interval rather than
//! reacting to in-process writes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use libsql::Builder;
use remote_host_admission::{AdmissionEntry, InMemoryAdmission};

/// Source-of-truth table: one row per admitted gateway instance key. `label` +
/// `created_at` are for whoever administers it. `max_conns` is the key's optional
/// per-key connection cap (NULL → the server's `MAX_CONNS_PER_INSTANCE` default)
/// and `max_bps` its optional relay-bandwidth ceiling in bytes/sec (NULL → the
/// relay's hardcoded `RELAY_BYTES_PER_SEC` default).
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS admitted_instances (\
    instance_key TEXT PRIMARY KEY, \
    label TEXT, \
    max_conns INTEGER, \
    max_bps INTEGER, \
    created_at TEXT NOT NULL DEFAULT (datetime('now')))";

/// Per-key limit columns added after the table's original shape. Each ALTER
/// errors harmlessly ("duplicate column") once present, so a re-run is a no-op.
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE admitted_instances ADD COLUMN max_conns INTEGER",
    "ALTER TABLE admitted_instances ADD COLUMN max_bps INTEGER",
];

/// Open the libsql DB at `path`, ensure the table, load the allow-list, and
/// spawn a task that re-reads it every `poll` to pick up external edits. On each
/// reload, `on_revoke` is called with the keys that just lost admission so their
/// live connections can be dropped. Returns the live admission handle shared by
/// both roles.
pub(crate) async fn open<F>(
    path: &str,
    poll: Duration,
    on_revoke: F,
) -> Result<Arc<InMemoryAdmission>, Box<dyn std::error::Error>>
where
    F: Fn(HashSet<String>) + Send + 'static,
{
    let db = Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    conn.execute(SCHEMA, ()).await?;
    // Bring a table created before the per-key limit columns existed up to date;
    // each ALTER is harmless once its column is present.
    for migration in MIGRATIONS {
        let _ = conn.execute(migration, ()).await;
    }

    let admission = Arc::new(InMemoryAdmission::new());
    // Initial load: nothing was admitted before, so nothing to revoke.
    let _ = admission.replace_all(load(&conn).await?);

    let admission_poll = admission.clone();
    tokio::spawn(async move {
        let _db = db; // keep the database alive for the connection
        let mut tick = tokio::time::interval(poll);
        tick.tick().await; // the first tick is immediate; we already loaded once
        loop {
            tick.tick().await;
            match load(&conn).await {
                Ok(keys) => {
                    let revoked = admission_poll.replace_all(keys);
                    // Drop the live connections of any key that just lost admission.
                    if !revoked.is_empty() {
                        on_revoke(revoked);
                    }
                }
                Err(e) => eprintln!("remote-host: admission poll failed: {e}"),
            }
        }
    });

    Ok(admission)
}

async fn load(conn: &libsql::Connection) -> Result<HashMap<String, AdmissionEntry>, libsql::Error> {
    let mut rows = conn
        .query(
            "SELECT instance_key, max_conns, max_bps FROM admitted_instances",
            (),
        )
        .await?;
    let mut keys = HashMap::new();
    while let Some(row) = rows.next().await? {
        let key = row.get::<String>(0)?;
        // Nullable INTEGERs -> per-key overrides; NULL or out-of-range -> None
        // (the caller's default applies).
        let max_conns = row
            .get::<Option<i64>>(1)?
            .and_then(|v| u32::try_from(v).ok());
        let max_bps = row
            .get::<Option<i64>>(2)?
            .and_then(|v| u64::try_from(v).ok());
        keys.insert(key, AdmissionEntry { max_conns, max_bps });
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DB created before the per-key limit columns existed must migrate cleanly
    /// and `load` (the widened SELECT) must read every column. Guards the
    /// migration↔SELECT contract: if the two ever drift, `open` fails on every
    /// upgraded deployment.
    #[tokio::test]
    async fn migrates_an_old_shape_db_and_loads_every_limit_column() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let conn = db.connect().unwrap();
        // The original table shape (instance_key, label, max_conns, created_at).
        conn.execute(
            "CREATE TABLE admitted_instances (instance_key TEXT PRIMARY KEY, label TEXT, \
             max_conns INTEGER, created_at TEXT)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO admitted_instances(instance_key, max_conns) VALUES('legacy', 7)",
            (),
        )
        .await
        .unwrap();

        // Apply the migrations open() runs, then load via the widened SELECT.
        for migration in MIGRATIONS {
            let _ = conn.execute(migration, ()).await;
        }
        let loaded = load(&conn).await.unwrap();
        let legacy = loaded.get("legacy").expect("the old-shape row is admitted");
        assert_eq!(legacy.max_conns, Some(7));
        assert_eq!(
            legacy.max_bps, None,
            "added column defaults to NULL -> no override"
        );

        // A row using the new column round-trips through load().
        conn.execute(
            "INSERT INTO admitted_instances(instance_key, max_bps) VALUES('tuned', 4194304)",
            (),
        )
        .await
        .unwrap();
        let tuned = load(&conn).await.unwrap();
        assert_eq!(tuned.get("tuned").unwrap().max_bps, Some(4_194_304));
    }
}
