use std::collections::HashMap;

use async_trait::async_trait;
use aura_model::ChannelType;
use chrono::Utc;

use super::LibsqlPool;
use crate::StorageError;
use crate::channel_bot::{ChannelBotRow, ChannelBotStore, Result};

pub struct LibsqlChannelBotStore {
    pool: LibsqlPool,
}

impl LibsqlChannelBotStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

fn parse_metadata(raw: &str) -> HashMap<String, String> {
    if raw.is_empty() {
        return HashMap::new();
    }
    serde_json::from_str(raw).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "channel_bots.metadata is not valid JSON; falling back to empty");
        HashMap::new()
    })
}

fn serialize_metadata(
    metadata: &HashMap<String, String>,
) -> std::result::Result<String, StorageError> {
    serde_json::to_string(metadata)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("serialize metadata: {e}")))
}

#[async_trait]
impl ChannelBotStore for LibsqlChannelBotStore {
    async fn list_live(&self, channel_type: &ChannelType) -> Result<Vec<ChannelBotRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT channel_type, bot_id, created_at, metadata FROM channel_bots
                 WHERE channel_type = ?1 AND deleted_at IS NULL
                 ORDER BY created_at DESC",
                libsql::params![channel_type.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let channel_type_s: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let bot_id: String = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let created_at: i64 = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let metadata_raw: String = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            out.push(ChannelBotRow {
                channel_type: ChannelType::from(channel_type_s.as_str()),
                bot_id,
                created_at,
                metadata: parse_metadata(&metadata_raw),
            });
        }
        Ok(out)
    }

    async fn get(&self, channel_type: &ChannelType, bot_id: &str) -> Result<Option<ChannelBotRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT channel_type, bot_id, created_at, metadata FROM channel_bots
                 WHERE channel_type = ?1 AND bot_id = ?2 AND deleted_at IS NULL",
                libsql::params![channel_type.as_str().to_string(), bot_id.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        else {
            return Ok(None);
        };
        let channel_type_s: String = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
        let bot_id: String = row
            .get(1)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
        let created_at: i64 = row
            .get(2)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
        let metadata_raw: String = row
            .get(3)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
        Ok(Some(ChannelBotRow {
            channel_type: ChannelType::from(channel_type_s.as_str()),
            bot_id,
            created_at,
            metadata: parse_metadata(&metadata_raw),
        }))
    }

    async fn put(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        metadata: HashMap<String, String>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let metadata_json = serialize_metadata(&metadata)?;
        let conn = self.pool.conn();
        conn.execute(
            "INSERT INTO channel_bots (channel_type, bot_id, created_at, metadata, deleted_at)
             VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(channel_type, bot_id) DO UPDATE SET
                 created_at = CASE
                     WHEN channel_bots.deleted_at IS NULL THEN channel_bots.created_at
                     ELSE excluded.created_at
                 END,
                 metadata = excluded.metadata,
                 deleted_at = NULL",
            libsql::params![
                channel_type.as_str().to_string(),
                bot_id.to_string(),
                now,
                metadata_json,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql upsert: {e}")))?;
        Ok(())
    }

    async fn delete(&self, channel_type: &ChannelType, bot_id: &str) -> Result<()> {
        let now = Utc::now().timestamp();
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE channel_bots
             SET deleted_at = ?3
             WHERE channel_type = ?1 AND bot_id = ?2 AND deleted_at IS NULL",
            libsql::params![channel_type.as_str().to_string(), bot_id.to_string(), now,],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_empty_by_default() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelBotStore::new(pool);
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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelBotStore::new(pool);
        store
            .put(&ChannelType::telegram(), "alpha", HashMap::new())
            .await
            .unwrap();
        store
            .put(&ChannelType::telegram(), "beta", HashMap::new())
            .await
            .unwrap();

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelBotStore::new(pool);
        store
            .put(&ChannelType::telegram(), "alpha", HashMap::new())
            .await
            .unwrap();
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
        store
            .put(&ChannelType::telegram(), "alpha", HashMap::new())
            .await
            .unwrap();
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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelBotStore::new(pool);
        store
            .put(&ChannelType::telegram(), "alpha", HashMap::new())
            .await
            .unwrap();
        let first = store
            .get(&ChannelType::telegram(), "alpha")
            .await
            .unwrap()
            .unwrap();
        // Second put — created_at must not change on a live-row conflict.
        store
            .put(&ChannelType::telegram(), "alpha", HashMap::new())
            .await
            .unwrap();
        let second = store
            .get(&ChannelType::telegram(), "alpha")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.created_at, second.created_at);
    }

    #[tokio::test]
    async fn channel_types_are_isolated() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelBotStore::new(pool);
        store
            .put(&ChannelType::telegram(), "x", HashMap::new())
            .await
            .unwrap();
        store
            .put(&ChannelType::discord(), "x", HashMap::new())
            .await
            .unwrap();
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
