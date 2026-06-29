//! The admission allow-list, sourced from a SQLite (libsql) table and
//! hot-reloaded by polling — the table may be updated out of band (another
//! process writes it), so the runtime re-reads it on an interval rather than
//! reacting to in-process writes.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use libsql::Builder;
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

    async fn mem_conn() -> libsql::Connection {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(SCHEMA, ()).await.unwrap();
        conn
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
}
