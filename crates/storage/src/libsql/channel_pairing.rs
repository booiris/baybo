use async_trait::async_trait;
use aura_model::ChannelType;

use super::LibsqlPool;
use crate::channel_pairing::{ChannelPairingRow, ChannelPairingStore, PairingStatus};

pub struct LibsqlChannelPairingStore {
    pool: LibsqlPool,
}

impl LibsqlChannelPairingStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

async fn fetch_row(rows: &mut libsql::Rows) -> Result<Option<ChannelPairingRow>, String> {
    let Some(row) = rows.next().await.map_err(|e| format!("libsql row: {e}"))? else {
        return Ok(None);
    };
    let ct: String = row.get(0).map_err(|e| format!("libsql get ct: {e}"))?;
    let bot_id: String = row.get(1).map_err(|e| format!("libsql get bot_id: {e}"))?;
    let user_id: String = row.get(2).map_err(|e| format!("libsql get user_id: {e}"))?;
    let code: String = row.get(3).map_err(|e| format!("libsql get code: {e}"))?;
    let status_s: String = row.get(4).map_err(|e| format!("libsql get status: {e}"))?;
    let status = PairingStatus::parse(&status_s)
        .ok_or_else(|| format!("unknown pairing status: {status_s}"))?;
    let created_at: i64 = row
        .get(5)
        .map_err(|e| format!("libsql get created_at: {e}"))?;
    let expires_at: Option<i64> = row
        .get(6)
        .map_err(|e| format!("libsql get expires_at: {e}"))?;
    let approved_at: Option<i64> = row
        .get(7)
        .map_err(|e| format!("libsql get approved_at: {e}"))?;
    Ok(Some(ChannelPairingRow {
        channel_type: ChannelType::from(ct.as_str()),
        bot_id,
        user_id,
        code,
        status,
        created_at,
        expires_at,
        approved_at,
    }))
}

#[async_trait]
impl ChannelPairingStore for LibsqlChannelPairingStore {
    async fn get(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<Option<ChannelPairingRow>, String> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT channel_type, bot_id, user_id, code, status,
                        created_at, expires_at, approved_at
                 FROM channel_pairings
                 WHERE channel_type = ?1 AND bot_id = ?2 AND user_id = ?3
                       AND deleted_at IS NULL",
                libsql::params![
                    channel_type.as_str().to_string(),
                    bot_id.to_string(),
                    user_id.to_string(),
                ],
            )
            .await
            .map_err(|e| format!("libsql query: {e}"))?;
        fetch_row(&mut rows).await
    }

    async fn get_by_code(&self, code: &str) -> Result<Option<ChannelPairingRow>, String> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT channel_type, bot_id, user_id, code, status,
                        created_at, expires_at, approved_at
                 FROM channel_pairings
                 WHERE code = ?1 AND deleted_at IS NULL",
                libsql::params![code.to_string()],
            )
            .await
            .map_err(|e| format!("libsql query: {e}"))?;
        fetch_row(&mut rows).await
    }

    async fn upsert_pending(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
        code: &str,
        now: i64,
        expires_at: i64,
    ) -> Result<ChannelPairingRow, String> {
        let conn = self.pool.conn();
        // Live-and-fresh rows win: an existing pending row whose
        // `expires_at > now` keeps its code/state so concurrent
        // inbounds from the same user agree. Expired pending rows and
        // tombstoned rows get overwritten with the new code and a
        // fresh expires_at.
        conn.execute(
            "INSERT INTO channel_pairings
                 (channel_type, bot_id, user_id, code, status,
                  created_at, expires_at, approved_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, NULL, NULL)
             ON CONFLICT(channel_type, bot_id, user_id) DO UPDATE SET
                 code = CASE
                     WHEN channel_pairings.deleted_at IS NULL
                          AND channel_pairings.status = 'pending'
                          AND channel_pairings.expires_at IS NOT NULL
                          AND channel_pairings.expires_at > ?5
                     THEN channel_pairings.code
                     ELSE excluded.code
                 END,
                 status = CASE
                     WHEN channel_pairings.deleted_at IS NULL
                          AND channel_pairings.status = 'pending'
                          AND channel_pairings.expires_at IS NOT NULL
                          AND channel_pairings.expires_at > ?5
                     THEN channel_pairings.status
                     ELSE excluded.status
                 END,
                 created_at = CASE
                     WHEN channel_pairings.deleted_at IS NULL
                          AND channel_pairings.status = 'pending'
                          AND channel_pairings.expires_at IS NOT NULL
                          AND channel_pairings.expires_at > ?5
                     THEN channel_pairings.created_at
                     ELSE excluded.created_at
                 END,
                 expires_at = CASE
                     WHEN channel_pairings.deleted_at IS NULL
                          AND channel_pairings.status = 'pending'
                          AND channel_pairings.expires_at IS NOT NULL
                          AND channel_pairings.expires_at > ?5
                     THEN channel_pairings.expires_at
                     ELSE excluded.expires_at
                 END,
                 approved_at = CASE
                     WHEN channel_pairings.deleted_at IS NULL
                          AND channel_pairings.status = 'pending'
                          AND channel_pairings.expires_at IS NOT NULL
                          AND channel_pairings.expires_at > ?5
                     THEN channel_pairings.approved_at
                     ELSE NULL
                 END,
                 deleted_at = NULL",
            libsql::params![
                channel_type.as_str().to_string(),
                bot_id.to_string(),
                user_id.to_string(),
                code.to_string(),
                now,
                expires_at,
            ],
        )
        .await
        .map_err(|e| format!("libsql upsert: {e}"))?;

        self.get(channel_type, bot_id, user_id)
            .await?
            .ok_or_else(|| "pairing row missing immediately after upsert_pending".to_string())
    }

    async fn approve_by_code(
        &self,
        code: &str,
        now: i64,
    ) -> Result<Option<ChannelPairingRow>, String> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE channel_pairings
                 SET status = 'approved',
                     approved_at = ?2,
                     expires_at = NULL
                 WHERE code = ?1
                       AND deleted_at IS NULL
                       AND status = 'pending'
                       AND (expires_at IS NULL OR expires_at > ?2)",
                libsql::params![code.to_string(), now],
            )
            .await
            .map_err(|e| format!("libsql update: {e}"))?;
        if affected == 0 {
            return Ok(None);
        }
        self.get_by_code(code).await
    }

    async fn list(&self, status: Option<PairingStatus>) -> Result<Vec<ChannelPairingRow>, String> {
        let conn = self.pool.conn();
        let mut rows = match status {
            Some(s) => {
                conn.query(
                    "SELECT channel_type, bot_id, user_id, code, status,
                            created_at, expires_at, approved_at
                     FROM channel_pairings
                     WHERE deleted_at IS NULL AND status = ?1
                     ORDER BY created_at DESC",
                    libsql::params![s.as_str().to_string()],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT channel_type, bot_id, user_id, code, status,
                            created_at, expires_at, approved_at
                     FROM channel_pairings
                     WHERE deleted_at IS NULL
                     ORDER BY created_at DESC",
                    libsql::params![],
                )
                .await
            }
        }
        .map_err(|e| format!("libsql query: {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = fetch_row(&mut rows).await? {
            out.push(row);
        }
        Ok(out)
    }

    async fn delete(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<(), String> {
        let now = super::time::now_us();
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE channel_pairings
             SET deleted_at = ?4
             WHERE channel_type = ?1 AND bot_id = ?2 AND user_id = ?3
                   AND deleted_at IS NULL",
            libsql::params![
                channel_type.as_str().to_string(),
                bot_id.to_string(),
                user_id.to_string(),
                now,
            ],
        )
        .await
        .map_err(|e| format!("libsql delete: {e}"))?;
        Ok(())
    }

    async fn purge_expired(&self, now_secs: i64, approved_cutoff_secs: i64) -> Result<u64, String> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "DELETE FROM channel_pairings \
                 WHERE (status = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?1) \
                    OR (status = 'approved' AND approved_at IS NOT NULL AND approved_at < ?2)",
                libsql::params![now_secs, approved_cutoff_secs],
            )
            .await
            .map_err(|e| format!("libsql delete: {e}"))?;
        Ok(affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT: &str = "telegram";
    const BOT: &str = "prod-bot";
    const UID: &str = "tg_prodbot_42_12345";

    fn ch() -> ChannelType {
        ChannelType::from(CT)
    }

    #[tokio::test]
    async fn upsert_then_get() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        let row = store
            .upsert_pending(&ch(), BOT, UID, "AB1234", 100, 1000)
            .await
            .unwrap();
        assert_eq!(row.code, "AB1234");
        assert_eq!(row.status, PairingStatus::Pending);
        assert_eq!(row.expires_at, Some(1000));

        let got = store.get(&ch(), BOT, UID).await.unwrap().unwrap();
        assert_eq!(got.code, "AB1234");

        let by_code = store.get_by_code("AB1234").await.unwrap().unwrap();
        assert_eq!(by_code.user_id, UID);
    }

    #[tokio::test]
    async fn upsert_on_live_fresh_row_preserves_code() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), BOT, UID, "AAAAAA", 100, 1000)
            .await
            .unwrap();
        let racer = store
            .upsert_pending(&ch(), BOT, UID, "BBBBBB", 200, 1200)
            .await
            .unwrap();
        assert_eq!(racer.code, "AAAAAA", "existing live code must survive");
        assert_eq!(racer.created_at, 100);
    }

    #[tokio::test]
    async fn upsert_overwrites_expired_pending() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), BOT, UID, "OLDCOD", 100, 200)
            .await
            .unwrap();
        // Now=300 is past the expires_at=200 — the old pending row
        // should be overwritten.
        let fresh = store
            .upsert_pending(&ch(), BOT, UID, "NEWCOD", 300, 1200)
            .await
            .unwrap();
        assert_eq!(fresh.code, "NEWCOD");
        assert_eq!(fresh.expires_at, Some(1200));
    }

    #[tokio::test]
    async fn approve_flips_status_and_clears_expiry() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), BOT, UID, "CODECD", 100, 1000)
            .await
            .unwrap();
        let approved = store.approve_by_code("CODECD", 200).await.unwrap().unwrap();
        assert_eq!(approved.status, PairingStatus::Approved);
        assert!(approved.approved_at.is_some());
        assert!(approved.expires_at.is_none());

        // Second approve on the same code is a no-op — already approved.
        let again = store.approve_by_code("CODECD", 300).await.unwrap();
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn approve_rejects_expired_code() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), BOT, UID, "EXPCOD", 100, 200)
            .await
            .unwrap();
        let out = store.approve_by_code("EXPCOD", 500).await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), BOT, "u1", "CODE01", 100, 1000)
            .await
            .unwrap();
        store
            .upsert_pending(&ch(), BOT, "u2", "CODE02", 110, 1000)
            .await
            .unwrap();
        store.approve_by_code("CODE01", 150).await.unwrap();

        let pending = store.list(Some(PairingStatus::Pending)).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].user_id, "u2");
        let approved = store.list(Some(PairingStatus::Approved)).await.unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].user_id, "u1");
        assert_eq!(store.list(None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn delete_hides_then_upsert_revives() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), BOT, UID, "FIRSTC", 100, 1000)
            .await
            .unwrap();
        store.approve_by_code("FIRSTC", 150).await.unwrap();
        store.delete(&ch(), BOT, UID).await.unwrap();
        assert!(store.get(&ch(), BOT, UID).await.unwrap().is_none());

        let revived = store
            .upsert_pending(&ch(), BOT, UID, "SECOND", 200, 2000)
            .await
            .unwrap();
        assert_eq!(revived.code, "SECOND");
        assert_eq!(revived.status, PairingStatus::Pending);
    }

    #[tokio::test]
    async fn different_bot_ids_do_not_collide() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), "bot-a", UID, "AAAAAA", 100, 1000)
            .await
            .unwrap();
        store
            .upsert_pending(&ch(), "bot-b", UID, "BBBBBB", 100, 1000)
            .await
            .unwrap();
        assert_eq!(
            store.get(&ch(), "bot-a", UID).await.unwrap().unwrap().code,
            "AAAAAA"
        );
        assert_eq!(
            store.get(&ch(), "bot-b", UID).await.unwrap().unwrap().code,
            "BBBBBB"
        );
    }
}
