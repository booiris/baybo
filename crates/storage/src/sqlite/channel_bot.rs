use async_trait::async_trait;
use baybo_model::ChannelType;
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::channel_bot::{ChannelBotRow, ChannelBotStore, Result};

pub struct SqliteChannelBotStore {
    pool: SqlitePool,
}

impl SqliteChannelBotStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ChannelBotStore for SqliteChannelBotStore {
    async fn list_live(&self, channel_type: &ChannelType) -> Result<Vec<ChannelBotRow>> {
        let channel_type = channel_type.as_str().to_string();
        self.pool
            .interact("channel_bots.list_live", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT channel_type, bot_id, created_at FROM channel_bots
                 WHERE channel_type = ?1
                 ORDER BY created_at DESC",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![channel_type], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows
                    .into_iter()
                    .map(|(channel_type_s, bot_id, created_at)| ChannelBotRow {
                        channel_type: ChannelType::from(channel_type_s.as_str()),
                        bot_id,
                        created_at,
                    })
                    .collect())
            })
            .await
    }

    async fn get(&self, channel_type: &ChannelType, bot_id: &str) -> Result<Option<ChannelBotRow>> {
        let channel_type = channel_type.as_str().to_string();
        let bot_id = bot_id.to_string();
        self.pool
            .interact("channel_bots.get", move |conn| {
                let row = conn
                    .query_row(
                        "SELECT channel_type, bot_id, created_at FROM channel_bots
                 WHERE channel_type = ?1 AND bot_id = ?2",
                        rusqlite::params![channel_type, bot_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                Ok(
                    row.map(|(channel_type_s, bot_id, created_at)| ChannelBotRow {
                        channel_type: ChannelType::from(channel_type_s.as_str()),
                        bot_id,
                        created_at,
                    }),
                )
            })
            .await
    }

    async fn put(&self, channel_type: &ChannelType, bot_id: &str) -> Result<()> {
        let now = super::time::now_us();
        let channel_type = channel_type.as_str().to_string();
        let bot_id = bot_id.to_string();
        self.pool
            .interact_write("channel_bots.put", move |conn| {
                // Live row wins: `INSERT OR IGNORE` preserves the existing
                // `created_at` on a re-add, no-op on conflict.
                conn.execute(
                    "INSERT OR IGNORE INTO channel_bots (channel_type, bot_id, created_at)
             VALUES (?1, ?2, ?3)",
                    rusqlite::params![channel_type, bot_id, now],
                )?;
                Ok(())
            })
            .await
    }

    async fn delete(&self, channel_type: &ChannelType, bot_id: &str) -> Result<()> {
        let channel_type = channel_type.as_str().to_string();
        let bot_id = bot_id.to_string();
        self.pool
            .interact_write("channel_bots.delete", move |conn| {
                conn.execute(
                    "DELETE FROM channel_bots
             WHERE channel_type = ?1 AND bot_id = ?2",
                    rusqlite::params![channel_type, bot_id],
                )?;
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_empty_by_default() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelBotStore::new(pool);
        assert!(
            store
                .list_live(&ChannelType::telegram())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn put_then_list_and_get() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelBotStore::new(pool);
        store.put(&ChannelType::telegram(), "alpha").await.unwrap();
        store.put(&ChannelType::telegram(), "beta").await.unwrap();

        let rows = store.list_live(&ChannelType::telegram()).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.bot_id.as_str()).collect();
        // list is newest-first; the second insert's created_at >= first's,
        // so `beta` is at the head on most clocks. Accept either order.
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"alpha"));
        assert!(ids.contains(&"beta"));

        let got = store
            .get(&ChannelType::telegram(), "alpha")
            .await
            .unwrap()
            .expect("alpha present");
        assert_eq!(got.bot_id, "alpha");
        assert_eq!(got.channel_type.as_str(), "telegram");
    }

    #[tokio::test]
    async fn delete_hides_then_put_revives() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelBotStore::new(pool);
        store.put(&ChannelType::telegram(), "alpha").await.unwrap();
        store
            .delete(&ChannelType::telegram(), "alpha")
            .await
            .unwrap();
        assert!(
            store
                .get(&ChannelType::telegram(), "alpha")
                .await
                .unwrap()
                .is_none()
        );
        store.put(&ChannelType::telegram(), "alpha").await.unwrap();
        assert!(
            store
                .get(&ChannelType::telegram(), "alpha")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn put_on_live_row_is_noop() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelBotStore::new(pool);
        store.put(&ChannelType::telegram(), "alpha").await.unwrap();
        let first = store
            .get(&ChannelType::telegram(), "alpha")
            .await
            .unwrap()
            .unwrap();
        // Second put — created_at must not change on a live-row conflict.
        store.put(&ChannelType::telegram(), "alpha").await.unwrap();
        let second = store
            .get(&ChannelType::telegram(), "alpha")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.created_at, second.created_at);
    }

    #[tokio::test]
    async fn channel_types_are_isolated() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelBotStore::new(pool);
        store.put(&ChannelType::telegram(), "x").await.unwrap();
        store.put(&ChannelType::discord(), "x").await.unwrap();
        assert_eq!(
            store
                .list_live(&ChannelType::telegram())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .list_live(&ChannelType::discord())
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
