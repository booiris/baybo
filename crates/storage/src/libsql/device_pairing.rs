use async_trait::async_trait;

use super::LibsqlPool;
use baybo_store::StorageError;
use baybo_store::device_pairing::{DevicePairingSlot, DevicePairingStore, Result};

pub struct LibsqlDevicePairingStore {
    pool: LibsqlPool,
}

impl LibsqlDevicePairingStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

const COLS: &str =
    "code, user_id, label, created_at, expires_at, confirm_code, device_id, operator_decision";

fn col_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("libsql {ctx}: {e}"))
}

async fn fetch_row(rows: &mut libsql::Rows) -> Result<Option<DevicePairingSlot>> {
    let Some(row) = rows.next().await.map_err(|e| col_err("row", e))? else {
        return Ok(None);
    };
    let operator_decision: Option<i64> = row
        .get(7)
        .map_err(|e| col_err("get operator_decision", e))?;
    Ok(Some(DevicePairingSlot {
        code: row.get(0).map_err(|e| col_err("get code", e))?,
        user_id: row.get(1).map_err(|e| col_err("get user_id", e))?,
        label: row.get(2).map_err(|e| col_err("get label", e))?,
        created_at: row.get(3).map_err(|e| col_err("get created_at", e))?,
        expires_at: row.get(4).map_err(|e| col_err("get expires_at", e))?,
        confirm_code: row.get(5).map_err(|e| col_err("get confirm_code", e))?,
        device_id: row.get(6).map_err(|e| col_err("get device_id", e))?,
        operator_decision: operator_decision.map(|v| v != 0),
    }))
}

#[async_trait]
impl DevicePairingStore for LibsqlDevicePairingStore {
    async fn create_slot(&self, slot: &DevicePairingSlot) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "INSERT INTO device_pairings (code, user_id, label, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            libsql::params![
                slot.code.clone(),
                slot.user_id.clone(),
                slot.label.clone(),
                slot.created_at,
                slot.expires_at,
            ],
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("constraint") || msg.contains("UNIQUE") {
                StorageError::Conflict(format!("pairing code {} already exists", slot.code))
            } else {
                col_err("insert slot", e)
            }
        })?;
        Ok(())
    }

    async fn get_slot(&self, code: &str) -> Result<Option<DevicePairingSlot>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!("SELECT {COLS} FROM device_pairings WHERE code = ?1"),
                libsql::params![code.to_string()],
            )
            .await
            .map_err(|e| col_err("query", e))?;
        fetch_row(&mut rows).await
    }

    async fn delete_slot(&self, code: &str) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "DELETE FROM device_pairings WHERE code = ?1",
                libsql::params![code.to_string()],
            )
            .await
            .map_err(|e| col_err("delete slot", e))?;
        Ok(affected > 0)
    }

    async fn set_confirm(
        &self,
        code: &str,
        confirm_code: &str,
        device_id: &str,
        label: &str,
    ) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE device_pairings SET confirm_code = ?2, device_id = ?3, label = ?4 WHERE code = ?1",
            libsql::params![
                code.to_string(),
                confirm_code.to_string(),
                device_id.to_string(),
                label.to_string(),
            ],
        )
        .await
        .map_err(|e| col_err("set confirm", e))?;
        Ok(())
    }

    async fn set_operator_decision(&self, code: &str, accepted: bool) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE device_pairings SET operator_decision = ?2 WHERE code = ?1",
            libsql::params![code.to_string(), i64::from(accepted)],
        )
        .await
        .map_err(|e| col_err("set operator decision", e))?;
        Ok(())
    }

    async fn list_slots(&self) -> Result<Vec<DevicePairingSlot>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!("SELECT {COLS} FROM device_pairings ORDER BY created_at DESC"),
                libsql::params![],
            )
            .await
            .map_err(|e| col_err("query list", e))?;
        let mut out = Vec::new();
        while let Some(row) = fetch_row(&mut rows).await? {
            out.push(row);
        }
        Ok(out)
    }

    async fn purge_expired(&self, now: i64) -> Result<u64> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "DELETE FROM device_pairings WHERE expires_at <= ?1",
                libsql::params![now],
            )
            .await
            .map_err(|e| col_err("purge", e))?;
        Ok(affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> LibsqlDevicePairingStore {
        LibsqlDevicePairingStore::new(LibsqlPool::open_in_memory().await.unwrap())
    }

    fn slot(code: &str, exp: i64) -> DevicePairingSlot {
        DevicePairingSlot {
            code: code.into(),
            user_id: "u1".into(),
            label: "iPhone".into(),
            created_at: 100,
            expires_at: exp,
            confirm_code: None,
            device_id: None,
            operator_decision: None,
        }
    }

    #[tokio::test]
    async fn create_get_delete() {
        let s = store().await;
        s.create_slot(&slot("ABC123", 1000)).await.unwrap();
        let got = s.get_slot("ABC123").await.unwrap().unwrap();
        assert_eq!(got.user_id, "u1");
        assert!(!got.is_expired(500));
        assert!(got.is_expired(1000));

        s.delete_slot("ABC123").await.unwrap();
        assert!(s.get_slot("ABC123").await.unwrap().is_none());
        // Delete of a gone slot is a no-op.
        s.delete_slot("ABC123").await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_code_conflicts() {
        let s = store().await;
        s.create_slot(&slot("DUP000", 1000)).await.unwrap();
        let err = s.create_slot(&slot("DUP000", 2000)).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn confirm_then_operator_decision_round_trip() {
        let s = store().await;
        s.create_slot(&slot("CONF01", 1000)).await.unwrap();
        // Fresh slot: no confirm challenge yet.
        let got = s.get_slot("CONF01").await.unwrap().unwrap();
        assert_eq!(got.confirm_code, None);
        assert_eq!(got.operator_decision, None);

        // Gateway publishes the challenge (with the device's reported label).
        s.set_confirm("CONF01", "123456", "dev-1", "Booiris iPhone")
            .await
            .unwrap();
        let got = s.get_slot("CONF01").await.unwrap().unwrap();
        assert_eq!(got.confirm_code.as_deref(), Some("123456"));
        assert_eq!(got.device_id.as_deref(), Some("dev-1"));
        assert_eq!(got.label, "Booiris iPhone");
        assert_eq!(got.operator_decision, None);

        // Operator declines, then (on a re-pair) approves — the value round-trips.
        s.set_operator_decision("CONF01", false).await.unwrap();
        assert_eq!(
            s.get_slot("CONF01")
                .await
                .unwrap()
                .unwrap()
                .operator_decision,
            Some(false)
        );
        s.set_operator_decision("CONF01", true).await.unwrap();
        assert_eq!(
            s.get_slot("CONF01")
                .await
                .unwrap()
                .unwrap()
                .operator_decision,
            Some(true)
        );
    }

    #[tokio::test]
    async fn purge_expired_removes_only_stale() {
        let s = store().await;
        s.create_slot(&slot("FRESH1", 1000)).await.unwrap();
        s.create_slot(&slot("STALE1", 200)).await.unwrap();
        let removed = s.purge_expired(300).await.unwrap();
        assert_eq!(removed, 1);
        assert!(s.get_slot("STALE1").await.unwrap().is_none());
        assert!(s.get_slot("FRESH1").await.unwrap().is_some());
        assert_eq!(s.list_slots().await.unwrap().len(), 1);
    }
}
