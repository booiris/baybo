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

fn parse_row(row: &libsql::Row) -> Result<ChannelBotRow> {
    let channel_type_s: String = row
        .get(0)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
    let bot_id: String = row
        .get(1)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
    let created_at: i64 = row
        .get(2)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
    let revision: i64 = row
        .get(3)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
    let metadata_raw: String = row
        .get(4)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
    Ok(ChannelBotRow {
        channel_type: ChannelType::from(channel_type_s.as_str()),
        bot_id,
        created_at,
        revision,
        metadata: parse_metadata(&metadata_raw),
    })
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
                "SELECT channel_type, bot_id, created_at, revision, metadata FROM channel_bots
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
            out.push(parse_row(&row)?);
        }
        Ok(out)
    }

    async fn get(&self, channel_type: &ChannelType, bot_id: &str) -> Result<Option<ChannelBotRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT channel_type, bot_id, created_at, revision, metadata FROM channel_bots
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
        Ok(Some(parse_row(&row)?))
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
        // The conflict branch bumps `revision` even when nothing
        // looks different in `metadata`: the bot reconciler treats a
        // revision change as "rotated, restart"; bumping
        // unconditionally is safe (a redundant restart) and keeps the
        // contract honest — every `put` is a re-registration intent.
        conn.execute(
            "INSERT INTO channel_bots (channel_type, bot_id, created_at, metadata, revision, deleted_at)
             VALUES (?1, ?2, ?3, ?4, 1, NULL)
             ON CONFLICT(channel_type, bot_id) DO UPDATE SET
                 created_at = CASE
                     WHEN channel_bots.deleted_at IS NULL THEN channel_bots.created_at
                     ELSE excluded.created_at
                 END,
                 metadata = excluded.metadata,
                 revision = channel_bots.revision + 1,
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
    async fn put_on_live_row_preserves_created_at_and_bumps_revision() {
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
        assert_eq!(first.revision, 1);
        // Second put — created_at must not change on a live-row
        // conflict, but revision must bump so the gateway reconciler
        // detects the rotation and restarts the running bot.
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
        assert_eq!(second.revision, 2);
    }

    /// Codex review regression: existing rows from before the
    /// revision column existed are filled with the column default
    /// (originally 0) by the ALTER. The follow-up UPDATE in
    /// `init_db` migrates them to 1 so the reconciler doesn't read
    /// `applied == 0` as "never started" and loop StartBot every
    /// tick. Simulates the upgrade path by directly inserting a
    /// pre-migration row with revision = 0 and asserting `get`
    /// observes the migrated value.
    #[tokio::test]
    async fn migration_lifts_zero_revision_rows_to_one() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        // Insert a row that simulates a pre-migration record (the
        // ALTER would have left it at the default of 0 if it landed
        // before the new UPDATE step). Bypassing `put` means we get
        // exactly the bug shape Codex flagged.
        pool.conn()
            .execute(
                "INSERT INTO channel_bots (channel_type, bot_id, created_at, metadata, revision, deleted_at)
                 VALUES ('telegram', 'pre-migration', 1700000000, '{}', 0, NULL)",
                (),
            )
            .await
            .unwrap();
        // Sanity-check that the row is at revision 0 right after the
        // direct insert (i.e. the test setup actually reproduces the
        // pre-migration shape).
        let mut rows = pool
            .conn()
            .query(
                "SELECT revision FROM channel_bots WHERE bot_id = 'pre-migration'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let direct_rev: i64 = row.get(0).unwrap();
        assert_eq!(direct_rev, 0, "direct insert should land at 0");

        // Re-running init_db is the migration path on every gateway
        // boot; the second invocation must lift the row to 1.
        // `LibsqlPool::open_in_memory` already called init_db once
        // during construction, but our INSERT happened after — so we
        // call init_db again to mirror the upgrade-after-restart
        // sequence.
        crate::libsql::LibsqlPool::reinit_db_for_test(&pool)
            .await
            .unwrap();

        let store = LibsqlChannelBotStore::new(pool);
        let migrated = store
            .get(&ChannelType::telegram(), "pre-migration")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            migrated.revision, 1,
            "migration must lift revision-0 rows to 1 to break the reconciler StartBot loop",
        );
    }

    #[tokio::test]
    async fn revive_after_delete_bumps_revision() {
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
        store
            .delete(&ChannelType::telegram(), "alpha")
            .await
            .unwrap();
        store
            .put(&ChannelType::telegram(), "alpha", HashMap::new())
            .await
            .unwrap();
        let revived = store
            .get(&ChannelType::telegram(), "alpha")
            .await
            .unwrap()
            .unwrap();
        // Revision continues from where it left off — the reconciler
        // sees a strictly larger value and restarts the bot, which is
        // the right behaviour after a soft-delete revive (the bot may
        // have been rotated while it was tombstoned).
        assert!(revived.revision > first.revision);
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
