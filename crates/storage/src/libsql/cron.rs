use async_trait::async_trait;

use super::LibsqlPool;
use crate::cron::{CronExecutionRow, CronJobRow, CronStore, CronStoreError};

pub struct LibsqlCronStore {
    pool: LibsqlPool,
}

impl LibsqlCronStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

fn read_job_row(row: &libsql::Row) -> crate::cron::Result<CronJobRow> {
    let s = |i| {
        row.get::<String>(i)
            .map_err(|e| CronStoreError::Internal(format!("libsql get column {i}: {e}")))
    };
    let n = |i| {
        row.get::<i64>(i)
            .map_err(|e| CronStoreError::Internal(format!("libsql get column {i}: {e}")))
    };
    Ok(CronJobRow {
        id: s(0)?,
        user_id: s(1)?,
        status: s(2)?,
        next_trigger_at: n(3)?,
        data: s(4)?,
    })
}

fn read_execution_row(row: &libsql::Row) -> crate::cron::Result<CronExecutionRow> {
    let s = |i| {
        row.get::<String>(i)
            .map_err(|e| CronStoreError::Internal(format!("libsql get column {i}: {e}")))
    };
    let n = |i| {
        row.get::<i64>(i)
            .map_err(|e| CronStoreError::Internal(format!("libsql get column {i}: {e}")))
    };
    Ok(CronExecutionRow {
        id: s(0)?,
        job_id: s(1)?,
        user_id: s(2)?,
        scheduled_fire_time: n(3)?,
        triggered_at: n(4)?,
        status: s(5)?,
        data: s(6)?,
    })
}

#[async_trait]
impl CronStore for LibsqlCronStore {
    async fn create(&self, row: &CronJobRow) -> crate::cron::Result<()> {
        self.pool
            .conn()
            .execute(
                "INSERT INTO cron_jobs (id, user_id, status, next_trigger_at, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                libsql::params![
                    row.id.clone(),
                    row.user_id.clone(),
                    row.status.clone(),
                    row.next_trigger_at,
                    row.data.clone(),
                ],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql insert: {e}")))?;
        Ok(())
    }

    async fn get(&self, job_id: &str) -> crate::cron::Result<Option<CronJobRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs WHERE id = ?1 AND deleted_at IS NULL",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        match rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            Some(row) => Ok(Some(read_job_row(&row)?)),
            None => Ok(None),
        }
    }

    async fn save(&self, row: &CronJobRow) -> crate::cron::Result<()> {
        let affected = self
            .pool
            .conn()
            .execute(
                "UPDATE cron_jobs SET user_id = ?1, status = ?2, next_trigger_at = ?3, data = ?4 \
                 WHERE id = ?5 AND deleted_at IS NULL",
                libsql::params![
                    row.user_id.clone(),
                    row.status.clone(),
                    row.next_trigger_at,
                    row.data.clone(),
                    row.id.clone(),
                ],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql update: {e}")))?;

        if affected == 0 {
            return Err(CronStoreError::NotFound(row.id.clone()));
        }
        Ok(())
    }

    async fn delete(&self, job_id: &str) -> crate::cron::Result<()> {
        let now = super::time::now_us();
        let affected = self
            .pool
            .conn()
            .execute(
                "UPDATE cron_jobs SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                libsql::params![now, job_id.to_string()],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql delete: {e}")))?;

        if affected == 0 {
            return Err(CronStoreError::NotFound(job_id.to_string()));
        }
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> crate::cron::Result<Vec<CronJobRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs WHERE user_id = ?1 AND deleted_at IS NULL",
                libsql::params![user_id.to_string()],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            out.push(read_job_row(&row)?);
        }
        Ok(out)
    }

    async fn list_all(&self) -> crate::cron::Result<Vec<CronJobRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs WHERE deleted_at IS NULL",
                (),
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            out.push(read_job_row(&row)?);
        }
        Ok(out)
    }

    async fn list_enabled(&self) -> crate::cron::Result<Vec<CronJobRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs WHERE status = 'enabled' AND deleted_at IS NULL",
                (),
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            out.push(read_job_row(&row)?);
        }
        Ok(out)
    }

    async fn list_due(&self, now_us: i64) -> crate::cron::Result<Vec<CronJobRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data FROM cron_jobs \
                 WHERE status = 'enabled' AND next_trigger_at != 0 AND next_trigger_at <= ?1 \
                 AND deleted_at IS NULL",
                libsql::params![now_us],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            out.push(read_job_row(&row)?);
        }
        Ok(out)
    }

    // ── Execution records ──

    async fn record_execution(&self, row: &CronExecutionRow) -> crate::cron::Result<()> {
        self.pool
            .conn()
            .execute(
                "INSERT INTO cron_executions (id, job_id, user_id, scheduled_fire_time, triggered_at, status, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                libsql::params![
                    row.id.clone(),
                    row.job_id.clone(),
                    row.user_id.clone(),
                    row.scheduled_fire_time,
                    row.triggered_at,
                    row.status.clone(),
                    row.data.clone(),
                ],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql insert: {e}")))?;
        Ok(())
    }

    async fn list_executions_by_job(
        &self,
        job_id: &str,
    ) -> crate::cron::Result<Vec<CronExecutionRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE job_id = ?1 AND deleted_at IS NULL ORDER BY triggered_at DESC",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            out.push(read_execution_row(&row)?);
        }
        Ok(out)
    }

    async fn list_executions_by_user(
        &self,
        user_id: &str,
    ) -> crate::cron::Result<Vec<CronExecutionRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE user_id = ?1 AND deleted_at IS NULL ORDER BY triggered_at DESC",
                libsql::params![user_id.to_string()],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            out.push(read_execution_row(&row)?);
        }
        Ok(out)
    }

    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time_us: i64,
    ) -> crate::cron::Result<bool> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT 1 FROM cron_executions WHERE job_id = ?1 AND scheduled_fire_time = ?2 LIMIT 1",
                libsql::params![job_id.to_string(), scheduled_fire_time_us],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let exists = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
            .is_some();
        Ok(exists)
    }

    async fn update_execution_status(
        &self,
        execution_id: &str,
        status: &str,
    ) -> crate::cron::Result<()> {
        let affected = self
            .pool
            .conn()
            .execute(
                "UPDATE cron_executions SET status = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                libsql::params![status.to_string(), execution_id.to_string()],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql update: {e}")))?;

        if affected == 0 {
            return Err(CronStoreError::NotFound(execution_id.to_string()));
        }
        Ok(())
    }

    async fn list_executions_by_status(
        &self,
        status: &str,
    ) -> crate::cron::Result<Vec<CronExecutionRow>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE status = ?1 AND deleted_at IS NULL",
                libsql::params![status.to_string()],
            )
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronStoreError::Internal(format!("libsql row: {e}")))?
        {
            out.push(read_execution_row(&row)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2099-01-01T00:00:00Z in Unix ms.
    const FUTURE_US: i64 = 4_070_908_800_000_000;

    fn test_row(id: &str, user_id: &str, status: &str) -> CronJobRow {
        CronJobRow {
            id: id.to_string(),
            user_id: user_id.to_string(),
            status: status.to_string(),
            next_trigger_at: FUTURE_US,
            data: format!(r#"{{"id":"{id}","user_id":"{user_id}"}}"#),
        }
    }

    fn test_exec_row(id: &str, job_id: &str, user_id: &str) -> CronExecutionRow {
        // Salt the schedule slot with the id's hash so the
        // UNIQUE(job_id, scheduled_fire_time) constraint isn't tripped
        // by repeat fixture rows.
        let salt: i64 = id.chars().map(|c| c as i64).sum();
        CronExecutionRow {
            id: id.to_string(),
            job_id: job_id.to_string(),
            user_id: user_id.to_string(),
            scheduled_fire_time: FUTURE_US + salt,
            triggered_at: FUTURE_US,
            status: "pending".to_string(),
            data: format!(r#"{{"id":"{id}","job_id":"{job_id}"}}"#),
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        let row = test_row("cj-1", "u1", "enabled");
        store.create(&row).await.unwrap();
        let loaded = store.get("cj-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "cj-1");
        assert_eq!(loaded.user_id, "u1");
    }

    #[tokio::test]
    async fn save_updates_row() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        let mut row = test_row("cj-2", "u1", "enabled");
        store.create(&row).await.unwrap();

        row.status = "disabled".to_string();
        store.save(&row).await.unwrap();

        let loaded = store.get("cj-2").await.unwrap().unwrap();
        assert_eq!(loaded.status, "disabled");
    }

    #[tokio::test]
    async fn save_nonexistent_returns_not_found() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        let row = test_row("nonexistent", "u1", "enabled");
        let err = store.save(&row).await.unwrap_err();
        assert!(matches!(err, CronStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        store
            .create(&test_row("cj-3", "u1", "enabled"))
            .await
            .unwrap();
        store.delete("cj-3").await.unwrap();
        assert!(store.get("cj-3").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        let err = store.delete("nonexistent").await.unwrap_err();
        assert!(matches!(err, CronStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_by_user_filters_correctly() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        store
            .create(&test_row("cj-4", "u1", "enabled"))
            .await
            .unwrap();
        store
            .create(&test_row("cj-5", "u2", "enabled"))
            .await
            .unwrap();
        store
            .create(&test_row("cj-6", "u1", "disabled"))
            .await
            .unwrap();

        let u1 = store.list_by_user("u1").await.unwrap();
        assert_eq!(u1.len(), 2);
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        store
            .create(&test_row("cj-7", "u1", "enabled"))
            .await
            .unwrap();
        store
            .create(&test_row("cj-8", "u1", "disabled"))
            .await
            .unwrap();

        let enabled = store.list_enabled().await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "cj-7");
    }

    #[tokio::test]
    async fn list_due_returns_only_past_due() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);

        // 2000-01-01T00:00:00Z and 2025-01-01T00:00:00Z in Unix ms.
        const PAST_US: i64 = 946_684_800_000_000;
        const NOW_US: i64 = 1_735_689_600_000_000;

        let mut past_due = test_row("cj-9", "u1", "enabled");
        past_due.next_trigger_at = PAST_US;
        store.create(&past_due).await.unwrap();

        let future = test_row("cj-10", "u1", "enabled");
        store.create(&future).await.unwrap();

        let mut disabled = test_row("cj-11", "u1", "disabled");
        disabled.next_trigger_at = PAST_US;
        store.create(&disabled).await.unwrap();

        let due = store.list_due(NOW_US).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "cj-9");
    }

    // ── Execution record tests ──

    #[tokio::test]
    async fn record_and_list_executions_by_job() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);

        store
            .record_execution(&test_exec_row("ce-1", "cj-1", "u1"))
            .await
            .unwrap();
        store
            .record_execution(&test_exec_row("ce-2", "cj-1", "u1"))
            .await
            .unwrap();
        store
            .record_execution(&test_exec_row("ce-3", "cj-2", "u1"))
            .await
            .unwrap();

        let execs = store.list_executions_by_job("cj-1").await.unwrap();
        assert_eq!(execs.len(), 2);
    }

    #[tokio::test]
    async fn list_executions_by_user() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);

        store
            .record_execution(&test_exec_row("ce-4", "cj-1", "u1"))
            .await
            .unwrap();
        store
            .record_execution(&test_exec_row("ce-5", "cj-2", "u2"))
            .await
            .unwrap();
        store
            .record_execution(&test_exec_row("ce-6", "cj-3", "u1"))
            .await
            .unwrap();

        let u1 = store.list_executions_by_user("u1").await.unwrap();
        assert_eq!(u1.len(), 2);
    }

    #[tokio::test]
    async fn execution_records_survive_job_deletion() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);

        store
            .create(&test_row("cj-evict", "u1", "enabled"))
            .await
            .unwrap();
        store
            .record_execution(&test_exec_row("ce-7", "cj-evict", "u1"))
            .await
            .unwrap();

        store.delete("cj-evict").await.unwrap();
        assert!(store.get("cj-evict").await.unwrap().is_none());

        let execs = store.list_executions_by_job("cj-evict").await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "ce-7");
    }
}
