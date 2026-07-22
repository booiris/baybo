use async_trait::async_trait;
use baybo_model::ChannelType;
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::channel_pairing::{ChannelPairingRow, ChannelPairingStore, PairingStatus};

pub struct SqliteChannelPairingStore {
    pool: SqlitePool,
}

impl SqliteChannelPairingStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Raw column tuple: the status string can't be parsed inside the rusqlite row
/// closure (a parse failure isn't a `rusqlite::Error`), so decoding happens
/// after the rows are collected.
type RawPairing = (
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawPairing> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn decode_row(raw: RawPairing) -> anyhow::Result<ChannelPairingRow> {
    let (ct, bot_id, user_id, code, status_s, created_at, expires_at, approved_at) = raw;
    let status = PairingStatus::parse(&status_s)
        .ok_or_else(|| anyhow::anyhow!("unknown pairing status: {status_s}"))?;
    Ok(ChannelPairingRow {
        channel_type: ChannelType::from(ct.as_str()),
        bot_id,
        user_id,
        code,
        status,
        created_at,
        expires_at,
        approved_at,
    })
}

#[async_trait]
impl ChannelPairingStore for SqliteChannelPairingStore {
    async fn get(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<Option<ChannelPairingRow>, String> {
        let ct = channel_type.as_str().to_string();
        let bot_id = bot_id.to_string();
        let user_id = user_id.to_string();
        self.pool
            .interact("channel_pairings.get", move |conn| {
                let raw = conn
                    .query_row(
                        "SELECT channel_type, bot_id, user_id, code, status,
                                created_at, expires_at, approved_at
                         FROM channel_pairings
                         WHERE channel_type = ?1 AND bot_id = ?2 AND user_id = ?3",
                        rusqlite::params![ct, bot_id, user_id],
                        read_row,
                    )
                    .optional()?;
                raw.map(decode_row).transpose()
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_by_code(&self, code: &str) -> Result<Option<ChannelPairingRow>, String> {
        let code = code.to_string();
        self.pool
            .interact("channel_pairings.get_by_code", move |conn| {
                let raw = conn
                    .query_row(
                        "SELECT channel_type, bot_id, user_id, code, status,
                                created_at, expires_at, approved_at
                         FROM channel_pairings
                         WHERE code = ?1",
                        rusqlite::params![code],
                        read_row,
                    )
                    .optional()?;
                raw.map(decode_row).transpose()
            })
            .await
            .map_err(|e| e.to_string())
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
        let ct = channel_type.as_str().to_string();
        let bot = bot_id.to_string();
        let uid = user_id.to_string();
        let code_owned = code.to_string();
        // Live-and-fresh rows win: an existing pending row whose
        // `expires_at > now` keeps its code/state so concurrent
        // inbounds from the same user agree. Expired pending rows
        // get overwritten with the new code and a fresh expires_at.
        self.pool
            .interact("channel_pairings.upsert_pending", move |conn| {
                conn.execute(
                    "INSERT INTO channel_pairings
                         (channel_type, bot_id, user_id, code, status,
                          created_at, expires_at, approved_at)
                     VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, NULL)
                     ON CONFLICT(channel_type, bot_id, user_id) DO UPDATE SET
                         code = CASE
                             WHEN channel_pairings.status = 'pending'
                                  AND channel_pairings.expires_at IS NOT NULL
                                  AND channel_pairings.expires_at > ?5
                             THEN channel_pairings.code
                             ELSE excluded.code
                         END,
                         status = CASE
                             WHEN channel_pairings.status = 'pending'
                                  AND channel_pairings.expires_at IS NOT NULL
                                  AND channel_pairings.expires_at > ?5
                             THEN channel_pairings.status
                             ELSE excluded.status
                         END,
                         created_at = CASE
                             WHEN channel_pairings.status = 'pending'
                                  AND channel_pairings.expires_at IS NOT NULL
                                  AND channel_pairings.expires_at > ?5
                             THEN channel_pairings.created_at
                             ELSE excluded.created_at
                         END,
                         expires_at = CASE
                             WHEN channel_pairings.status = 'pending'
                                  AND channel_pairings.expires_at IS NOT NULL
                                  AND channel_pairings.expires_at > ?5
                             THEN channel_pairings.expires_at
                             ELSE excluded.expires_at
                         END,
                         approved_at = CASE
                             WHEN channel_pairings.status = 'pending'
                                  AND channel_pairings.expires_at IS NOT NULL
                                  AND channel_pairings.expires_at > ?5
                             THEN channel_pairings.approved_at
                             ELSE NULL
                         END",
                    rusqlite::params![ct, bot, uid, code_owned, now, expires_at],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?;

        self.get(channel_type, bot_id, user_id)
            .await?
            .ok_or_else(|| "pairing row missing immediately after upsert_pending".to_string())
    }

    async fn approve_by_code(
        &self,
        code: &str,
        now: i64,
    ) -> Result<Option<ChannelPairingRow>, String> {
        let code_owned = code.to_string();
        let affected = self
            .pool
            .interact("channel_pairings.approve_by_code", move |conn| {
                Ok(conn.execute(
                    "UPDATE channel_pairings
                     SET status = 'approved',
                         approved_at = ?2,
                         expires_at = NULL
                     WHERE code = ?1
                           AND status = 'pending'
                           AND (expires_at IS NULL OR expires_at > ?2)",
                    rusqlite::params![code_owned, now],
                )?)
            })
            .await
            .map_err(|e| e.to_string())?;
        if affected == 0 {
            return Ok(None);
        }
        self.get_by_code(code).await
    }

    async fn list(&self, status: Option<PairingStatus>) -> Result<Vec<ChannelPairingRow>, String> {
        self.pool
            .interact("channel_pairings.list", move |conn| {
                let raws: Vec<RawPairing> = match status {
                    Some(s) => {
                        let mut stmt = conn.prepare(
                            "SELECT channel_type, bot_id, user_id, code, status,
                                    created_at, expires_at, approved_at
                             FROM channel_pairings
                             WHERE status = ?1
                             ORDER BY created_at DESC",
                        )?;

                        stmt.query_map(rusqlite::params![s.as_str().to_string()], read_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    }
                    None => {
                        let mut stmt = conn.prepare(
                            "SELECT channel_type, bot_id, user_id, code, status,
                                    created_at, expires_at, approved_at
                             FROM channel_pairings
                             ORDER BY created_at DESC",
                        )?;

                        stmt.query_map([], read_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    }
                };
                raws.into_iter().map(decode_row).collect()
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<(), String> {
        let ct = channel_type.as_str().to_string();
        let bot_id = bot_id.to_string();
        let user_id = user_id.to_string();
        self.pool
            .interact("channel_pairings.delete", move |conn| {
                conn.execute(
                    "DELETE FROM channel_pairings
                     WHERE channel_type = ?1 AND bot_id = ?2 AND user_id = ?3",
                    rusqlite::params![ct, bot_id, user_id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn purge_expired(&self, now_secs: i64, approved_cutoff_secs: i64) -> Result<u64, String> {
        self.pool
            .interact("channel_pairings.purge_expired", move |conn| {
                let affected = conn.execute(
                    "DELETE FROM channel_pairings \
                     WHERE (status = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?1) \
                        OR (status = 'approved' AND approved_at IS NOT NULL AND approved_at < ?2)",
                    rusqlite::params![now_secs, approved_cutoff_secs],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e| e.to_string())
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
        store
            .upsert_pending(&ch(), BOT, UID, "EXPCOD", 100, 200)
            .await
            .unwrap();
        let out = store.approve_by_code("EXPCOD", 500).await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelPairingStore::new(pool);
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
