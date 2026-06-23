use async_trait::async_trait;

use super::LibsqlPool;
use aura_store::device_pairing::{DevicePairingSlot, DevicePairingStore, Result};
use aura_store::StorageError;

pub struct LibsqlDevicePairingStore {
    pool: LibsqlPool,
}

impl LibsqlDevicePairingStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

const COLS: &str = "code, user_id, label, created_at, expires_at";

fn col_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("libsql {ctx}: {e}"))
}

async fn fetch_row(rows: &mut libsql::Rows) -> Result<Option<DevicePairingSlot>> {
    let Some(row) = rows.next().await.map_err(|e| col_err("row", e))? else {
        return Ok(None);
    };
    Ok(Some(DevicePairingSlot {
        code: row.get(0).map_err(|e| col_err("get code", e))?,
        user_id: row.get(1).map_err(|e| col_err("get user_id", e))?,
        label: row.get(2).map_err(|e| col_err("get label", e))?,
        created_at: row.get(3).map_err(|e| col_err("get created_at", e))?,
        expires_at: row.get(4).map_err(|e| col_err("get expires_at", e))?,
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

    async fn delete_slot(&self, code: &str) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "DELETE FROM device_pairings WHERE code = ?1",
            libsql::params![code.to_string()],
        )
        .await
        .map_err(|e| col_err("delete slot", e))?;
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
