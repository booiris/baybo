use async_trait::async_trait;

use super::LibsqlPool;
use baybo_store::StorageError;
use baybo_store::device::{DeviceRow, DeviceStatus, DeviceStore, Result};

pub struct LibsqlDeviceStore {
    pool: LibsqlPool,
}

impl LibsqlDeviceStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

const COLS: &str = "device_id, device_pubkey, auth_token, status, \
     rendezvous_id, created_at, approved_at, last_seen_at, relay_url, remote_api_key";

/// Shared INSERT for a device row — reused by `create` and the transactional
/// `create_replacing_approved` so the column list has one source of truth.
const INSERT_DEVICE: &str = "INSERT INTO devices
     (device_id, device_pubkey, auth_token, status,
      rendezvous_id, created_at, approved_at, last_seen_at, relay_url, remote_api_key)
 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

/// Re-pair tail appended to [`INSERT_DEVICE`], used only by
/// [`LibsqlDeviceStore::create_replacing_approved`]. `device_id` is a stable,
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
     remote_api_key = excluded.remote_api_key";

fn col_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("libsql {ctx}: {e}"))
}

/// Map a libsql INSERT error to a [`StorageError::Conflict`] when it tripped a
/// uniqueness constraint (duplicate `device_id` or `auth_token`), else a generic
/// internal error.
fn insert_conflict_err(row: &DeviceRow, e: impl std::fmt::Display) -> StorageError {
    let msg = e.to_string();
    if msg.contains("constraint") || msg.contains("UNIQUE") {
        StorageError::Conflict(format!(
            "device {} or its auth_token already exists",
            row.device_id
        ))
    } else {
        col_err("insert device", e)
    }
}

/// The eight positional params for [`INSERT_DEVICE`], in column order.
fn insert_params(row: &DeviceRow) -> Vec<libsql::Value> {
    use libsql::Value;
    let opt_int = |v: Option<i64>| v.map_or(Value::Null, Value::Integer);
    vec![
        Value::Text(row.device_id.clone()),
        Value::Blob(row.device_pubkey.clone()),
        Value::Text(row.auth_token.clone()),
        Value::Text(row.status.as_str().to_string()),
        row.rendezvous_id.clone().map_or(Value::Null, Value::Text),
        Value::Integer(row.created_at),
        opt_int(row.approved_at),
        opt_int(row.last_seen_at),
        Value::Text(row.relay_url.clone()),
        Value::Text(row.remote_api_key.clone()),
    ]
}

async fn fetch_row(rows: &mut libsql::Rows) -> Result<Option<DeviceRow>> {
    let Some(row) = rows.next().await.map_err(|e| col_err("row", e))? else {
        return Ok(None);
    };
    let status_s: String = row.get(3).map_err(|e| col_err("get status", e))?;
    let status = DeviceStatus::parse(&status_s)
        .ok_or_else(|| col_err("status", format!("unknown device status: {status_s}")))?;
    Ok(Some(DeviceRow {
        device_id: row.get(0).map_err(|e| col_err("get device_id", e))?,
        device_pubkey: row.get(1).map_err(|e| col_err("get device_pubkey", e))?,
        auth_token: row.get(2).map_err(|e| col_err("get auth_token", e))?,
        status,
        rendezvous_id: row.get(4).map_err(|e| col_err("get rendezvous_id", e))?,
        created_at: row.get(5).map_err(|e| col_err("get created_at", e))?,
        approved_at: row.get(6).map_err(|e| col_err("get approved_at", e))?,
        last_seen_at: row.get(7).map_err(|e| col_err("get last_seen_at", e))?,
        relay_url: row.get(8).map_err(|e| col_err("get relay_url", e))?,
        remote_api_key: row.get(9).map_err(|e| col_err("get remote_api_key", e))?,
    }))
}

async fn collect(rows: &mut libsql::Rows) -> Result<Vec<DeviceRow>> {
    let mut out = Vec::new();
    while let Some(row) = fetch_row(rows).await? {
        out.push(row);
    }
    Ok(out)
}

#[async_trait]
impl DeviceStore for LibsqlDeviceStore {
    async fn create(&self, row: &DeviceRow) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(INSERT_DEVICE, insert_params(row))
            .await
            .map_err(|e| insert_conflict_err(row, e))?;
        Ok(())
    }

    async fn create_replacing_approved(&self, row: &DeviceRow) -> Result<Vec<String>> {
        let conn = self.pool.conn();
        // BEGIN IMMEDIATE takes the write lock up front so the revoke + upsert
        // commit as a unit — no window where there are zero or two approved
        // devices, and the partial unique index can never trip.
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .map_err(|e| col_err("begin replace tx", e))?;

        // The devices we're about to supersede, captured for the operator's
        // "replaced X" report before they're flipped to revoked.
        let mut rows = tx
            .query(
                "SELECT device_id FROM devices WHERE status = 'approved'",
                libsql::params![],
            )
            .await
            .map_err(|e| col_err("query approved", e))?;
        let mut replaced = Vec::new();
        while let Some(r) = rows.next().await.map_err(|e| col_err("row", e))? {
            let id = r
                .get::<String>(0)
                .map_err(|e| col_err("get device_id", e))?;
            // A same-device re-pair (stable device_id) refreshes its own row in
            // place below; it didn't supersede a *different* binding, so don't
            // report it as replaced.
            if id != row.device_id {
                replaced.push(id);
            }
        }
        drop(rows);

        tx.execute(
            "UPDATE devices SET status = 'revoked' WHERE status = 'approved'",
            libsql::params![],
        )
        .await
        .map_err(|e| col_err("revoke prior approved", e))?;

        tx.execute(
            &format!("{INSERT_DEVICE}{REPAIR_UPSERT_TAIL}"),
            insert_params(row),
        )
        .await
        .map_err(|e| insert_conflict_err(row, e))?;

        tx.commit()
            .await
            .map_err(|e| col_err("commit replace tx", e))?;
        Ok(replaced)
    }

    async fn get(&self, device_id: &str) -> Result<Option<DeviceRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!("SELECT {COLS} FROM devices WHERE device_id = ?1"),
                libsql::params![device_id.to_string()],
            )
            .await
            .map_err(|e| col_err("query", e))?;
        fetch_row(&mut rows).await
    }

    async fn lookup_approved_by_auth_token(&self, auth_token: &str) -> Result<Option<DeviceRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {COLS} FROM devices WHERE auth_token = ?1 AND status = 'approved'"
                ),
                libsql::params![auth_token.to_string()],
            )
            .await
            .map_err(|e| col_err("query by token", e))?;
        fetch_row(&mut rows).await
    }

    async fn lookup_approved_by_pubkey(&self, device_pubkey: &[u8]) -> Result<Option<DeviceRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                // `ORDER BY ... LIMIT 1` keeps the result deterministic even in
                // the (cryptographically impossible) event two approved rows
                // shared a static key, so device resolution never flaps.
                &format!(
                    "SELECT {COLS} FROM devices WHERE device_pubkey = ?1 AND status = 'approved' \
                     ORDER BY created_at ASC LIMIT 1"
                ),
                libsql::params![device_pubkey.to_vec()],
            )
            .await
            .map_err(|e| col_err("query by pubkey", e))?;
        fetch_row(&mut rows).await
    }

    async fn list(&self, status: Option<DeviceStatus>) -> Result<Vec<DeviceRow>> {
        let conn = self.pool.conn();
        let mut rows = match status {
            Some(s) => {
                conn.query(
                    &format!(
                        "SELECT {COLS} FROM devices WHERE status = ?1 ORDER BY created_at DESC"
                    ),
                    libsql::params![s.as_str().to_string()],
                )
                .await
            }
            None => {
                conn.query(
                    &format!("SELECT {COLS} FROM devices ORDER BY created_at DESC"),
                    libsql::params![],
                )
                .await
            }
        }
        .map_err(|e| col_err("query list", e))?;
        collect(&mut rows).await
    }

    async fn revoke(&self, device_id: &str) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE devices SET status = 'revoked'
                 WHERE device_id = ?1 AND status != 'revoked'",
                libsql::params![device_id.to_string()],
            )
            .await
            .map_err(|e| col_err("revoke", e))?;
        Ok(affected > 0)
    }

    async fn touch_last_seen(&self, device_id: &str, now: i64) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE devices SET last_seen_at = ?2 WHERE device_id = ?1",
            libsql::params![device_id.to_string(), now],
        )
        .await
        .map_err(|e| col_err("touch", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_row(device: &str, token: &str, code: &str) -> DeviceRow {
        DeviceRow {
            device_id: device.into(),
            device_pubkey: vec![7u8; 32],
            auth_token: token.into(),
            status: DeviceStatus::Approved,
            rendezvous_id: Some(code.into()),
            created_at: 100,
            approved_at: Some(100),
            last_seen_at: None,
            relay_url: "wss://relay.test".into(),
            remote_api_key: "inst-test".into(),
        }
    }

    async fn store() -> LibsqlDeviceStore {
        LibsqlDeviceStore::new(LibsqlPool::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn create_then_get_round_trips_blob() {
        let s = store().await;
        s.create(&device_row("d1", "tok1", "ABC123")).await.unwrap();
        let got = s.get("d1").await.unwrap().unwrap();
        assert_eq!(got.device_id, "d1");
        assert_eq!(got.device_pubkey, vec![7u8; 32]);
        assert_eq!(got.status, DeviceStatus::Approved);
        assert_eq!(got.rendezvous_id.as_deref(), Some("ABC123"));
    }

    #[tokio::test]
    async fn duplicate_auth_token_conflicts() {
        let s = store().await;
        s.create(&device_row("d1", "shared", "C1")).await.unwrap();
        let err = s
            .create(&device_row("d2", "shared", "C2"))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn approved_token_authenticates_until_revoked() {
        let s = store().await;
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
        let s = store().await;
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
        let s = store().await;
        s.create(&device_row("d1", "tok1", "CODE12")).await.unwrap();
        assert!(s.revoke("d1").await.unwrap());

        // Row survives (audit), token no longer authenticates.
        let row = s.get("d1").await.unwrap().unwrap();
        assert_eq!(row.status, DeviceStatus::Revoked);
        assert_eq!(row.auth_token, "tok1", "token slot retained, not reused");
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
        let s = store().await;
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
        let s = store().await;
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
        let s = store().await;
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
        assert_eq!(approved[0].auth_token, "tok2", "token refreshed in place");
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
        let s = store().await;
        s.create(&device_row("d1", "tok1", "C1")).await.unwrap();
        // A bare `create` of a second approved row trips the partial unique
        // index — the backstop behind `create_replacing_approved`.
        let err = s.create(&device_row("d2", "tok2", "C2")).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn touch_last_seen_updates() {
        let s = store().await;
        s.create(&device_row("d1", "t1", "C1")).await.unwrap();
        s.touch_last_seen("d1", 555).await.unwrap();
        assert_eq!(s.get("d1").await.unwrap().unwrap().last_seen_at, Some(555));
    }
}
