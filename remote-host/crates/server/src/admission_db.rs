//! The admission allow-list, sourced from a SQLite (libsql) table and
//! hot-reloaded by polling — the table may be updated out of band (another
//! process writes it), so the runtime re-reads it on an interval rather than
//! reacting to in-process writes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use libsql::Builder;
use remote_host_admission::InMemoryAdmission;

/// Source-of-truth table: one row per admitted gateway instance key. `label` +
/// `created_at` are for whoever administers it; `max_conns` is the key's optional
/// per-key connection cap (NULL → the server's `MAX_CONNS_PER_INSTANCE` default).
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS admitted_instances (\
    instance_key TEXT PRIMARY KEY, \
    label TEXT, \
    max_conns INTEGER, \
    created_at TEXT NOT NULL DEFAULT (datetime('now')))";

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
    // Migrate a table created before the per-key cap column existed. The ALTER
    // errors harmlessly ("duplicate column") once it's present; ignore that.
    let _ = conn
        .execute("ALTER TABLE admitted_instances ADD COLUMN max_conns INTEGER", ())
        .await;

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

async fn load(conn: &libsql::Connection) -> Result<HashMap<String, Option<u32>>, libsql::Error> {
    let mut rows = conn
        .query("SELECT instance_key, max_conns FROM admitted_instances", ())
        .await?;
    let mut keys = HashMap::new();
    while let Some(row) = rows.next().await? {
        let key = row.get::<String>(0)?;
        // Nullable INTEGER -> per-key cap override; NULL or out-of-range -> None
        // (the caller's default applies).
        let cap = row.get::<Option<i64>>(1)?.and_then(|v| u32::try_from(v).ok());
        keys.insert(key, cap);
    }
    Ok(keys)
}
