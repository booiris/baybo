use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::device::{DeviceRow, DeviceStatus, DeviceStore, Result, hash_auth_token};

pub struct SqliteDeviceStore {
    pool: SqlitePool,
}

impl SqliteDeviceStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const COLS: &str = "device_id, device_pubkey, auth_token, status, \
     rendezvous_id, created_at, approved_at, last_seen_at, relay_url, push_url, remote_api_key";

/// Shared INSERT for a device row — reused by `create` and the transactional
/// `create_replacing_approved` so the column list has one source of truth.
const INSERT_DEVICE: &str = "INSERT INTO devices
     (device_id, device_pubkey, auth_token, status,
      rendezvous_id, created_at, approved_at, last_seen_at, relay_url, push_url, remote_api_key)
 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

/// Re-pair tail appended to [`INSERT_DEVICE`], used only by
/// [`SqliteDeviceStore::create_replacing_approved`]. `device_id` is a stable,
/// client-persisted identity (the app keys it off a keychain-pinned Noise
/// static), so re-pairing the **same** physical device collides on the
/// `device_id` primary key. Upsert refreshes that row in place — new token /
/// pubkey / status — instead of erroring; `created_at` is left untouched so it
/// stays the device's first-seen time. Pairing a **different** device hits no
/// conflict and inserts a fresh row as before.
const REPAIR_UPSERT_TAIL: &str = " ON CONFLICT(device_id) DO UPDATE SET \
     device_pubkey = excluded.device_pubkey, \
     auth_token = excluded.auth_token, \
     status = excluded.status, \
     rendezvous_id = excluded.rendezvous_id, \
     approved_at = excluded.approved_at, \
     last_seen_at = excluded.last_seen_at, \
     relay_url = excluded.relay_url, \
     push_url = excluded.push_url, \
     remote_api_key = excluded.remote_api_key";

/// Map an INSERT error to a [`StorageError::Conflict`] when it tripped a
/// uniqueness constraint (duplicate `device_id` or `auth_token`), else a generic
/// internal error. `StorageError::Conflict` cannot be built inside the pool's
/// `anyhow` closure, so the failing statement's message is carried out and
/// classified here.
fn insert_conflict_err(op: &str, device_id: &str, msg: &str) -> StorageError {
    if msg.contains("constraint") || msg.contains("UNIQUE") {
        StorageError::Conflict(format!(
            "device {device_id} or its auth_token already exists"
        ))
    } else {
        StorageError::Internal(anyhow::anyhow!("{op}: insert device: {msg}"))
    }
}

/// The eleven positional params for [`INSERT_DEVICE`], in column order.
fn insert_params(row: &DeviceRow) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value;
    let opt_int = |v: Option<i64>| v.map_or(Value::Null, Value::Integer);
    vec![
        Value::Text(row.device_id.clone()),
        Value::Blob(row.device_pubkey.clone()),
        Value::Text(row.auth_token_sha256.clone()),
        Value::Text(row.status.as_str().to_string()),
        row.rendezvous_id.clone().map_or(Value::Null, Value::Text),
        Value::Integer(row.created_at),
        opt_int(row.approved_at),
        opt_int(row.last_seen_at),
        Value::Text(row.relay_url.clone()),
        Value::Text(row.push_url.clone()),
        Value::Text(row.remote_api_key.clone()),
    ]
}

/// The raw column tuple for [`COLS`], decoded inside a `rusqlite` row closure.
/// `status` stays a `String` here: parsing it can fail with a non-rusqlite
/// error, so it happens after the rows are collected.
struct RawDevice {
    device_id: String,
    device_pubkey: Vec<u8>,
    auth_token_sha256: String,
    status: String,
    rendezvous_id: Option<String>,
    created_at: i64,
    approved_at: Option<i64>,
    last_seen_at: Option<i64>,
    relay_url: String,
    push_url: String,
    remote_api_key: String,
}

fn raw_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawDevice> {
    Ok(RawDevice {
        device_id: row.get(0)?,
        device_pubkey: row.get(1)?,
        auth_token_sha256: row.get(2)?,
        status: row.get(3)?,
        rendezvous_id: row.get(4)?,
        created_at: row.get(5)?,
        approved_at: row.get(6)?,
        last_seen_at: row.get(7)?,
        relay_url: row.get(8)?,
        push_url: row.get(9)?,
        remote_api_key: row.get(10)?,
    })
}

fn into_device_row(raw: RawDevice) -> anyhow::Result<DeviceRow> {
    let status = DeviceStatus::parse(&raw.status)
        .ok_or_else(|| anyhow::anyhow!("unknown device status: {}", raw.status))?;
    Ok(DeviceRow {
        device_id: raw.device_id,
        device_pubkey: raw.device_pubkey,
        auth_token_sha256: raw.auth_token_sha256,
        status,
        rendezvous_id: raw.rendezvous_id,
        created_at: raw.created_at,
        approved_at: raw.approved_at,
        last_seen_at: raw.last_seen_at,
        relay_url: raw.relay_url,
        push_url: raw.push_url,
        remote_api_key: raw.remote_api_key,
    })
}

#[async_trait]
impl DeviceStore for SqliteDeviceStore {
    async fn create(&self, row: &DeviceRow) -> Result<()> {
        let device_id = row.device_id.clone();
        let params = insert_params(row);
        let failed = self
            .pool
            .interact_write("devices.create", move |conn| {
                match conn.execute(INSERT_DEVICE, rusqlite::params_from_iter(params.iter())) {
                    Ok(_) => Ok(None),
                    Err(e) => Ok(Some(e.to_string())),
                }
            })
            .await?;
        match failed {
            Some(msg) => Err(insert_conflict_err("devices.create", &device_id, &msg)),
            None => Ok(()),
        }
    }

    async fn create_provisioning(&self, row: &DeviceRow) -> Result<()> {
        let device_id = row.device_id.clone();
        let params = insert_params(row);
        let failed = self
            .pool
            .interact_write("devices.create_provisioning", move |conn| {
                let sql = format!("{INSERT_DEVICE}{REPAIR_UPSERT_TAIL}");
                match conn.execute(&sql, rusqlite::params_from_iter(params.iter())) {
                    Ok(_) => Ok(None),
                    Err(e) => Ok(Some(e.to_string())),
                }
            })
            .await?;
        match failed {
            Some(msg) => Err(insert_conflict_err(
                "devices.create_provisioning",
                &device_id,
                &msg,
            )),
            None => Ok(()),
        }
    }

    async fn approve_replacing(&self, device_id: &str, approved_at: i64) -> Result<Vec<String>> {
        let id = device_id.to_string();
        let replaced = self
            .pool
            .interact_write("devices.approve_replacing", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let exists = tx
                    .query_row(
                        "SELECT device_id FROM devices WHERE device_id = ?1",
                        rusqlite::params![id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .is_some();
                if !exists {
                    // Nothing to approve — drop the tx (rollback) and let the
                    // caller raise NotFound.
                    return Ok(None);
                }

                let replaced = {
                    let mut stmt = tx.prepare(
                        "SELECT device_id FROM devices WHERE status = 'approved' AND device_id != ?1",
                    )?;
                    stmt.query_map(rusqlite::params![id], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };

                tx.execute(
                    "UPDATE devices SET status = 'revoked'
             WHERE status = 'approved' AND device_id != ?1",
                    rusqlite::params![id],
                )?;

                tx.execute(
                    "UPDATE devices SET status = 'approved', approved_at = ?2
             WHERE device_id = ?1",
                    rusqlite::params![id, approved_at],
                )?;

                tx.commit()?;
                Ok(Some(replaced))
            })
            .await?;
        replaced.ok_or_else(|| StorageError::NotFound(format!("device {device_id}")))
    }

    async fn create_replacing_approved(&self, row: &DeviceRow) -> Result<Vec<String>> {
        let device_id = row.device_id.clone();
        let params = insert_params(row);
        let inserted_id = device_id.clone();
        let outcome = self
            .pool
            .interact_write("devices.create_replacing_approved", move |conn| {
                // BEGIN IMMEDIATE takes the write lock up front so the revoke + upsert
                // commit as a unit — no window where there are zero or two approved
                // devices, and the partial unique index can never trip.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // The devices we're about to supersede, captured for the operator's
                // "replaced X" report before they're flipped to revoked.
                let approved = {
                    let mut stmt =
                        tx.prepare("SELECT device_id FROM devices WHERE status = 'approved'")?;
                    stmt.query_map([], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                let mut replaced = Vec::new();
                for id in approved {
                    // A same-device re-pair (stable device_id) refreshes its own row in
                    // place below; it didn't supersede a *different* binding, so don't
                    // report it as replaced.
                    if id != inserted_id {
                        replaced.push(id);
                    }
                }

                tx.execute(
                    "UPDATE devices SET status = 'revoked' WHERE status = 'approved'",
                    [],
                )?;

                let sql = format!("{INSERT_DEVICE}{REPAIR_UPSERT_TAIL}");
                if let Err(e) = tx.execute(&sql, rusqlite::params_from_iter(params.iter())) {
                    // Drop the tx without committing: the revoke rolls back with it.
                    return Ok(Err(e.to_string()));
                }

                tx.commit()?;
                Ok(Ok(replaced))
            })
            .await?;
        outcome.map_err(|msg| {
            insert_conflict_err("devices.create_replacing_approved", &device_id, &msg)
        })
    }

    async fn get(&self, device_id: &str) -> Result<Option<DeviceRow>> {
        let id = device_id.to_string();
        self.pool
            .interact("devices.get", move |conn| {
                let raw = conn
                    .query_row(
                        &format!("SELECT {COLS} FROM devices WHERE device_id = ?1"),
                        rusqlite::params![id],
                        raw_from_row,
                    )
                    .optional()?;
                raw.map(into_device_row).transpose()
            })
            .await
    }

    async fn lookup_approved_by_auth_token(
        &self,
        presented_token: &str,
    ) -> Result<Option<DeviceRow>> {
        // Match on the digest, never the bearer. Equality on a hash is safe to
        // index and safe to compare non-constant-time: learning the stored
        // digest does not let an attacker authenticate, since presenting it
        // would only be hashed again.
        let token = hash_auth_token(presented_token);
        self.pool
            .interact("devices.lookup_approved_by_auth_token", move |conn| {
                let raw = conn
                    .query_row(
                        &format!(
                            "SELECT {COLS} FROM devices WHERE auth_token = ?1 AND status = 'approved'"
                        ),
                        rusqlite::params![token],
                        raw_from_row,
                    )
                    .optional()?;
                raw.map(into_device_row).transpose()
            })
            .await
    }

    async fn lookup_approved_by_pubkey(&self, device_pubkey: &[u8]) -> Result<Option<DeviceRow>> {
        let pubkey = device_pubkey.to_vec();
        self.pool
            .interact("devices.lookup_approved_by_pubkey", move |conn| {
                let raw = conn
                    .query_row(
                        // `ORDER BY ... LIMIT 1` keeps the result deterministic even in
                        // the (cryptographically impossible) event two approved rows
                        // shared a static key, so device resolution never flaps.
                        &format!(
                            "SELECT {COLS} FROM devices WHERE device_pubkey = ?1 AND status = 'approved' \
                             ORDER BY created_at ASC LIMIT 1"
                        ),
                        rusqlite::params![pubkey],
                        raw_from_row,
                    )
                    .optional()?;
                raw.map(into_device_row).transpose()
            })
            .await
    }

    async fn list(&self, status: Option<DeviceStatus>) -> Result<Vec<DeviceRow>> {
        self.pool
            .interact("devices.list", move |conn| {
                let raws = match status {
                    Some(s) => {
                        let mut stmt = conn.prepare(&format!(
                            "SELECT {COLS} FROM devices WHERE status = ?1 ORDER BY created_at DESC"
                        ))?;

                        stmt.query_map(rusqlite::params![s.as_str().to_string()], raw_from_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    }
                    None => {
                        let mut stmt = conn.prepare(&format!(
                            "SELECT {COLS} FROM devices ORDER BY created_at DESC"
                        ))?;

                        stmt.query_map([], raw_from_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    }
                };
                raws.into_iter().map(into_device_row).collect()
            })
            .await
    }

    async fn revoke(&self, device_id: &str) -> Result<bool> {
        let id = device_id.to_string();
        self.pool
            .interact_write("devices.revoke", move |conn| {
                let affected = conn.execute(
                    "UPDATE devices SET status = 'revoked'
                 WHERE device_id = ?1 AND status != 'revoked'",
                    rusqlite::params![id],
                )?;
                Ok(affected > 0)
            })
            .await
    }

    async fn touch_last_seen(&self, device_id: &str, now: i64) -> Result<()> {
        let id = device_id.to_string();
        self.pool
            .interact_write("devices.touch_last_seen", move |conn| {
                conn.execute(
                    "UPDATE devices SET last_seen_at = ?2 WHERE device_id = ?1",
                    rusqlite::params![id, now],
                )?;
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `token` is the **plaintext** a device would present; the fixture stores
    /// its digest, matching what the pairing path writes.
    fn device_row(device: &str, token: &str, code: &str) -> DeviceRow {
        DeviceRow {
            device_id: device.into(),
            device_pubkey: vec![7u8; 32],
            auth_token_sha256: hash_auth_token(token),
            status: DeviceStatus::Approved,
            rendezvous_id: Some(code.into()),
            created_at: 100,
            approved_at: Some(100),
            last_seen_at: None,
            relay_url: "wss://relay.test".into(),
            push_url: "https://push.test".into(),
            remote_api_key: "inst-test".into(),
        }
    }

    /// Hands back the `TempDir` too: dropping it deletes the database file
    /// and its `-wal`/`-shm` siblings out from under the live connections.
    async fn store() -> (tempfile::TempDir, SqliteDeviceStore) {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        (tmpdir, SqliteDeviceStore::new(pool))
    }

    #[tokio::test]
    async fn create_then_get_round_trips_blob() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "tok1", "ABC123")).await.unwrap();
        let got = s.get("d1").await.unwrap().unwrap();
        assert_eq!(got.device_id, "d1");
        assert_eq!(got.device_pubkey, vec![7u8; 32]);
        assert_eq!(got.status, DeviceStatus::Approved);
        assert_eq!(got.rendezvous_id.as_deref(), Some("ABC123"));
        assert_eq!(got.relay_url, "wss://relay.test");
        assert_eq!(got.push_url, "https://push.test");
        assert_eq!(got.remote_api_key, "inst-test");
    }

    #[tokio::test]
    async fn duplicate_auth_token_conflicts() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "shared", "C1")).await.unwrap();
        let err = s
            .create(&device_row("d2", "shared", "C2"))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn approved_token_authenticates_until_revoked() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "tok1", "CODE12")).await.unwrap();
        // A freshly-paired row exists only post-confirm, so it authenticates now.
        let resolved = s
            .lookup_approved_by_auth_token("tok1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.device_id, "d1");

        // Revoking kills the token.
        assert!(s.revoke("d1").await.unwrap());
        assert!(
            s.lookup_approved_by_auth_token("tok1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn lookup_by_pubkey_matches_only_approved() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "tok1", "C1")).await.unwrap(); // device_pubkey = [7u8; 32]
        let got = s
            .lookup_approved_by_pubkey(&[7u8; 32])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.device_id, "d1");
        // A non-matching key resolves to nothing.
        assert!(
            s.lookup_approved_by_pubkey(&[9u8; 32])
                .await
                .unwrap()
                .is_none()
        );
        // Revoking removes it from the approved-by-pubkey path too.
        assert!(s.revoke("d1").await.unwrap());
        assert!(
            s.lookup_approved_by_pubkey(&[7u8; 32])
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn revoke_keeps_row_but_kills_auth() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "tok1", "CODE12")).await.unwrap();
        assert!(s.revoke("d1").await.unwrap());

        // Row survives (audit), token no longer authenticates.
        let row = s.get("d1").await.unwrap().unwrap();
        assert_eq!(row.status, DeviceStatus::Revoked);
        assert_eq!(
            row.auth_token_sha256,
            hash_auth_token("tok1"),
            "token slot retained, not reused"
        );
        assert!(
            s.lookup_approved_by_auth_token("tok1")
                .await
                .unwrap()
                .is_none()
        );

        // Second revoke is a no-op.
        assert!(!s.revoke("d1").await.unwrap());
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "t1", "C1")).await.unwrap();
        // Replacing keeps the one-approved invariant: d1 → revoked, d2 →
        // approved. (A bare second `create` would trip the index.)
        s.create_replacing_approved(&device_row("d2", "t2", "C2"))
            .await
            .unwrap();

        // Exactly one approved device (d2); d1 is revoked.
        let approved = s.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].device_id, "d2");

        let revoked = s.list(Some(DeviceStatus::Revoked)).await.unwrap();
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].device_id, "d1");

        assert_eq!(s.list(None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn create_replacing_approved_supersedes_prior_device() {
        let (_tmpdir, s) = store().await;
        // First pairing: nothing to replace.
        let replaced = s
            .create_replacing_approved(&device_row("d1", "tok1", "C1"))
            .await
            .unwrap();
        assert!(replaced.is_empty(), "first pairing replaces nothing");

        // Second pairing supersedes the first.
        let replaced = s
            .create_replacing_approved(&device_row("d2", "tok2", "C2"))
            .await
            .unwrap();
        assert_eq!(
            replaced,
            vec!["d1".to_string()],
            "reports the superseded id"
        );

        // Exactly one approved device remains, and it's the new one.
        let approved = s.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(approved.len(), 1, "one approved device (1:1)");
        assert_eq!(approved[0].device_id, "d2");
        // The old row survives as revoked (audit), token no longer authenticates.
        assert_eq!(
            s.get("d1").await.unwrap().unwrap().status,
            DeviceStatus::Revoked
        );
        assert!(
            s.lookup_approved_by_auth_token("tok1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            s.lookup_approved_by_auth_token("tok2")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn create_replacing_approved_repairs_same_device_id_in_place() {
        let (_tmpdir, s) = store().await;
        // First pairing of device d1.
        s.create_replacing_approved(&device_row("d1", "tok1", "C1"))
            .await
            .unwrap();

        // Re-pairing the SAME device (device_id is now a stable client identity)
        // refreshes the row in place instead of colliding on device_id.
        let mut repair = device_row("d1", "tok2", "C2");
        repair.created_at = 200; // a later first-seen value that must NOT overwrite
        let replaced = s.create_replacing_approved(&repair).await.unwrap();
        assert!(
            replaced.is_empty(),
            "a same-device re-pair supersedes no *other* binding"
        );

        // Exactly one approved row, still d1, with the refreshed token.
        let approved = s.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(approved.len(), 1, "no stray second row");
        assert_eq!(approved[0].device_id, "d1");
        assert_eq!(
            approved[0].auth_token_sha256,
            hash_auth_token("tok2"),
            "token refreshed in place"
        );
        assert_eq!(
            approved[0].created_at, 100,
            "created_at preserved as the device's first-seen time"
        );
        // The superseded token no longer authenticates; the new one does.
        assert!(
            s.lookup_approved_by_auth_token("tok1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            s.lookup_approved_by_auth_token("tok2")
                .await
                .unwrap()
                .is_some()
        );
        // Still a single row total (in-place update, not append).
        assert_eq!(s.list(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_approved_index_blocks_a_second_approved_insert() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "tok1", "C1")).await.unwrap();
        // A bare `create` of a second approved row trips the partial unique
        // index — the backstop behind `create_replacing_approved`.
        let err = s.create(&device_row("d2", "tok2", "C2")).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn touch_last_seen_updates() {
        let (_tmpdir, s) = store().await;
        s.create(&device_row("d1", "t1", "C1")).await.unwrap();
        s.touch_last_seen("d1", 555).await.unwrap();
        assert_eq!(s.get("d1").await.unwrap().unwrap().last_seen_at, Some(555));
    }
}
