use async_trait::async_trait;

use super::LibsqlPool;
use aura_store::device::{DeviceRow, DeviceStatus, DeviceStore, Result};
use aura_store::StorageError;

pub struct LibsqlDeviceStore {
    pool: LibsqlPool,
}

impl LibsqlDeviceStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }

    async fn get_by_code(&self, code: &str) -> Result<Option<DeviceRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!("SELECT {COLS} FROM devices WHERE pairing_code = ?1"),
                libsql::params![code.to_string()],
            )
            .await
            .map_err(|e| col_err("query by code", e))?;
        fetch_row(&mut rows).await
    }
}

const COLS: &str = "user_id, device_id, label, device_pubkey, auth_token, status, \
     pairing_code, created_at, approved_at, last_seen_at";

fn col_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("libsql {ctx}: {e}"))
}

async fn fetch_row(rows: &mut libsql::Rows) -> Result<Option<DeviceRow>> {
    let Some(row) = rows.next().await.map_err(|e| col_err("row", e))? else {
        return Ok(None);
    };
    let status_s: String = row.get(5).map_err(|e| col_err("get status", e))?;
    let status = DeviceStatus::parse(&status_s)
        .ok_or_else(|| col_err("status", format!("unknown device status: {status_s}")))?;
    Ok(Some(DeviceRow {
        user_id: row.get(0).map_err(|e| col_err("get user_id", e))?,
        device_id: row.get(1).map_err(|e| col_err("get device_id", e))?,
        label: row.get(2).map_err(|e| col_err("get label", e))?,
        device_pubkey: row.get(3).map_err(|e| col_err("get device_pubkey", e))?,
        auth_token: row.get(4).map_err(|e| col_err("get auth_token", e))?,
        status,
        pairing_code: row.get(6).map_err(|e| col_err("get pairing_code", e))?,
        created_at: row.get(7).map_err(|e| col_err("get created_at", e))?,
        approved_at: row.get(8).map_err(|e| col_err("get approved_at", e))?,
        last_seen_at: row.get(9).map_err(|e| col_err("get last_seen_at", e))?,
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
        conn.execute(
            "INSERT INTO devices
                 (user_id, device_id, label, device_pubkey, auth_token, status,
                  pairing_code, created_at, approved_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            libsql::params![
                row.user_id.clone(),
                row.device_id.clone(),
                row.label.clone(),
                row.device_pubkey.clone(),
                row.auth_token.clone(),
                row.status.as_str().to_string(),
                row.pairing_code.clone(),
                row.created_at,
                row.approved_at,
                row.last_seen_at,
            ],
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("constraint") || msg.contains("UNIQUE") {
                StorageError::Conflict(format!(
                    "device ({}, {}) or its auth_token already exists",
                    row.user_id, row.device_id
                ))
            } else {
                col_err("insert device", e)
            }
        })?;
        Ok(())
    }

    async fn get(&self, user_id: &str, device_id: &str) -> Result<Option<DeviceRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!("SELECT {COLS} FROM devices WHERE user_id = ?1 AND device_id = ?2"),
                libsql::params![user_id.to_string(), device_id.to_string()],
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

    async fn list_for_user(
        &self,
        user_id: &str,
        status: Option<DeviceStatus>,
    ) -> Result<Vec<DeviceRow>> {
        let conn = self.pool.conn();
        let mut rows = match status {
            Some(s) => {
                conn.query(
                    &format!(
                        "SELECT {COLS} FROM devices \
                         WHERE user_id = ?1 AND status = ?2 ORDER BY created_at DESC"
                    ),
                    libsql::params![user_id.to_string(), s.as_str().to_string()],
                )
                .await
            }
            None => {
                conn.query(
                    &format!(
                        "SELECT {COLS} FROM devices WHERE user_id = ?1 ORDER BY created_at DESC"
                    ),
                    libsql::params![user_id.to_string()],
                )
                .await
            }
        }
        .map_err(|e| col_err("query list_for_user", e))?;
        collect(&mut rows).await
    }

    async fn approve_by_code(&self, code: &str, now: i64) -> Result<Option<DeviceRow>> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE devices SET status = 'approved', approved_at = ?2
                 WHERE pairing_code = ?1 AND status = 'pending'",
                libsql::params![code.to_string(), now],
            )
            .await
            .map_err(|e| col_err("approve", e))?;
        if affected == 0 {
            return Ok(None);
        }
        self.get_by_code(code).await
    }

    async fn revoke(&self, user_id: &str, device_id: &str) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE devices SET status = 'revoked'
                 WHERE user_id = ?1 AND device_id = ?2 AND status != 'revoked'",
                libsql::params![user_id.to_string(), device_id.to_string()],
            )
            .await
            .map_err(|e| col_err("revoke", e))?;
        Ok(affected > 0)
    }

    async fn touch_last_seen(&self, user_id: &str, device_id: &str, now: i64) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE devices SET last_seen_at = ?3 WHERE user_id = ?1 AND device_id = ?2",
            libsql::params![user_id.to_string(), device_id.to_string(), now],
        )
        .await
        .map_err(|e| col_err("touch", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_row(user: &str, device: &str, token: &str, code: &str) -> DeviceRow {
        DeviceRow {
            user_id: user.into(),
            device_id: device.into(),
            label: "iPhone".into(),
            device_pubkey: vec![7u8; 32],
            auth_token: token.into(),
            status: DeviceStatus::Pending,
            pairing_code: Some(code.into()),
            created_at: 100,
            approved_at: None,
            last_seen_at: None,
        }
    }

    async fn store() -> LibsqlDeviceStore {
        LibsqlDeviceStore::new(LibsqlPool::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn create_then_get_round_trips_blob() {
        let s = store().await;
        s.create(&pending_row("u1", "d1", "tok1", "ABC123"))
            .await
            .unwrap();
        let got = s.get("u1", "d1").await.unwrap().unwrap();
        assert_eq!(got.device_id, "d1");
        assert_eq!(got.device_pubkey, vec![7u8; 32]);
        assert_eq!(got.status, DeviceStatus::Pending);
        assert_eq!(got.pairing_code.as_deref(), Some("ABC123"));
    }

    #[tokio::test]
    async fn duplicate_auth_token_conflicts() {
        let s = store().await;
        s.create(&pending_row("u1", "d1", "shared", "C1"))
            .await
            .unwrap();
        let err = s
            .create(&pending_row("u2", "d2", "shared", "C2"))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn pending_token_does_not_authenticate_until_approved() {
        let s = store().await;
        s.create(&pending_row("u1", "d1", "tok1", "CODE12"))
            .await
            .unwrap();
        // Inert while pending.
        assert!(s
            .lookup_approved_by_auth_token("tok1")
            .await
            .unwrap()
            .is_none());

        let approved = s.approve_by_code("CODE12", 200).await.unwrap().unwrap();
        assert_eq!(approved.status, DeviceStatus::Approved);
        assert_eq!(approved.approved_at, Some(200));

        // Now it resolves.
        let resolved = s
            .lookup_approved_by_auth_token("tok1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.device_id, "d1");
    }

    #[tokio::test]
    async fn approve_unknown_or_nonpending_code_is_none() {
        let s = store().await;
        assert!(s.approve_by_code("NOPE", 1).await.unwrap().is_none());
        s.create(&pending_row("u1", "d1", "t", "CODE99"))
            .await
            .unwrap();
        s.approve_by_code("CODE99", 200).await.unwrap();
        // Second approve on the now-approved code is a no-op.
        assert!(s.approve_by_code("CODE99", 300).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn revoke_keeps_row_but_kills_auth() {
        let s = store().await;
        s.create(&pending_row("u1", "d1", "tok1", "CODE12"))
            .await
            .unwrap();
        s.approve_by_code("CODE12", 200).await.unwrap();
        assert!(s.revoke("u1", "d1").await.unwrap());

        // Row survives (audit), token no longer authenticates.
        let row = s.get("u1", "d1").await.unwrap().unwrap();
        assert_eq!(row.status, DeviceStatus::Revoked);
        assert_eq!(row.auth_token, "tok1", "token slot retained, not reused");
        assert!(s
            .lookup_approved_by_auth_token("tok1")
            .await
            .unwrap()
            .is_none());

        // Second revoke is a no-op.
        assert!(!s.revoke("u1", "d1").await.unwrap());
    }

    #[tokio::test]
    async fn list_and_list_for_user_filter_by_status() {
        let s = store().await;
        s.create(&pending_row("u1", "d1", "t1", "C1"))
            .await
            .unwrap();
        s.create(&pending_row("u1", "d2", "t2", "C2"))
            .await
            .unwrap();
        s.create(&pending_row("u2", "d3", "t3", "C3"))
            .await
            .unwrap();
        s.approve_by_code("C1", 200).await.unwrap();

        let approved = s.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].device_id, "d1");

        let u1_pending = s
            .list_for_user("u1", Some(DeviceStatus::Pending))
            .await
            .unwrap();
        assert_eq!(u1_pending.len(), 1);
        assert_eq!(u1_pending[0].device_id, "d2");

        assert_eq!(s.list(None).await.unwrap().len(), 3);
        assert_eq!(s.list_for_user("u1", None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn touch_last_seen_updates() {
        let s = store().await;
        s.create(&pending_row("u1", "d1", "t1", "C1"))
            .await
            .unwrap();
        s.touch_last_seen("u1", "d1", 555).await.unwrap();
        assert_eq!(s.get("u1", "d1").await.unwrap().unwrap().last_seen_at, Some(555));
    }
}
