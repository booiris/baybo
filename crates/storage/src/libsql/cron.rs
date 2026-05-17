//! libsql implementation of [`CronStore`].
//!
//! Cron rows persist as the same `(id, user_id, status, next_trigger_at,
//! data)` shape they always have — `data` carries the full
//! [`CronJob`] / [`CronExecution`] as JSON, and the queryable columns
//! are projected out at write time. The translation lives here (not
//! in the cron scheduler) so `aura-cron` can stay free of `aura-storage`.

use async_trait::async_trait;
use aura_cron::{CronError, CronExecution, CronJob, CronStore, ExecutionStatus, Result};

use super::LibsqlPool;

pub struct LibsqlCronStore {
    pool: LibsqlPool,
}

impl LibsqlCronStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

// ── Row helpers ───────────────────────────────────────────────────────

fn col_string(row: &libsql::Row, i: i32) -> Result<String> {
    row.get::<String>(i)
        .map_err(|e| CronError::Storage(format!("libsql get column {i}: {e}")))
}

fn job_from_row(row: &libsql::Row) -> Result<CronJob> {
    // Cols: (id, user_id, status, next_trigger_at, data)
    let data = col_string(row, 4)?;
    serde_json::from_str(&data)
        .map_err(|e| CronError::Storage(format!("failed to deserialize cron job: {e}")))
}

fn execution_from_row(row: &libsql::Row) -> Result<CronExecution> {
    // Cols: (id, job_id, user_id, scheduled_fire_time, triggered_at, status, data)
    let data = col_string(row, 6)?;
    let mut exec: CronExecution = serde_json::from_str(&data)
        .map_err(|e| CronError::Storage(format!("failed to deserialize execution: {e}")))?;
    // The row-level `status` column is the source of truth — `update_execution_status`
    // touches the column without rewriting `data`.
    let status_col = col_string(row, 5)?;
    exec.status = match status_col.as_str() {
        "dispatched" => ExecutionStatus::Dispatched,
        _ => ExecutionStatus::Pending,
    };
    Ok(exec)
}

fn execution_status_str(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Pending => "pending",
        ExecutionStatus::Dispatched => "dispatched",
    }
}

fn serialize_job(job: &CronJob) -> Result<String> {
    serde_json::to_string(job)
        .map_err(|e| CronError::Storage(format!("failed to serialize cron job: {e}")))
}

fn serialize_execution(exec: &CronExecution) -> Result<String> {
    serde_json::to_string(exec)
        .map_err(|e| CronError::Storage(format!("failed to serialize execution: {e}")))
}

#[async_trait]
impl CronStore for LibsqlCronStore {
    async fn create(&self, job: &CronJob) -> Result<()> {
        let data = serialize_job(job)?;
        let next_trigger_us = job
            .next_trigger_at
            .map(|t| t.timestamp_micros())
            .unwrap_or(0);
        self.pool
            .conn()
            .execute(
                "INSERT INTO cron_jobs (id, user_id, status, next_trigger_at, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                libsql::params![
                    job.id.clone(),
                    job.user_id.clone(),
                    job.status.as_str().to_string(),
                    next_trigger_us,
                    data,
                ],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql insert: {e}")))?;
        Ok(())
    }

    async fn get(&self, job_id: &str) -> Result<Option<CronJob>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs WHERE id = ?1",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        match rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            Some(row) => Ok(Some(job_from_row(&row)?)),
            None => Ok(None),
        }
    }

    async fn save(&self, job: &CronJob) -> Result<()> {
        let data = serialize_job(job)?;
        let next_trigger_us = job
            .next_trigger_at
            .map(|t| t.timestamp_micros())
            .unwrap_or(0);
        let affected = self
            .pool
            .conn()
            .execute(
                "UPDATE cron_jobs SET user_id = ?1, status = ?2, next_trigger_at = ?3, data = ?4 \
                 WHERE id = ?5",
                libsql::params![
                    job.user_id.clone(),
                    job.status.as_str().to_string(),
                    next_trigger_us,
                    data,
                    job.id.clone(),
                ],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql update: {e}")))?;

        if affected == 0 {
            return Err(CronError::NotFound(job.id.clone()));
        }
        Ok(())
    }

    async fn delete(&self, job_id: &str) -> Result<()> {
        let affected = self
            .pool
            .conn()
            .execute(
                "DELETE FROM cron_jobs WHERE id = ?1",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql delete: {e}")))?;

        if affected == 0 {
            return Err(CronError::NotFound(job_id.to_string()));
        }
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs WHERE user_id = ?1",
                libsql::params![user_id.to_string()],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            out.push(job_from_row(&row)?);
        }
        Ok(out)
    }

    async fn list_all(&self) -> Result<Vec<CronJob>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs",
                (),
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            out.push(job_from_row(&row)?);
        }
        Ok(out)
    }

    async fn list_enabled(&self) -> Result<Vec<CronJob>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data \
                 FROM cron_jobs WHERE status = 'enabled'",
                (),
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            out.push(job_from_row(&row)?);
        }
        Ok(out)
    }

    async fn list_due(&self, now_us: i64) -> Result<Vec<CronJob>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, user_id, status, next_trigger_at, data FROM cron_jobs \
                 WHERE status = 'enabled' AND next_trigger_at != 0 AND next_trigger_at <= ?1",
                libsql::params![now_us],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            out.push(job_from_row(&row)?);
        }
        Ok(out)
    }

    // ── Execution records ──

    async fn record_execution(&self, exec: &CronExecution) -> Result<()> {
        let data = serialize_execution(exec)?;
        let scheduled_us = exec.scheduled_fire_time.timestamp_micros();
        let triggered_us = exec.triggered_at.timestamp_micros();
        // INSERT OR IGNORE so the (job_id, scheduled_fire_time) unique
        // index distinguishes "lost the dedup race" from "DB is broken".
        // 0 affected rows means another scheduler beat us to this slot;
        // the caller treats that as benign and skips the dispatch.
        let affected = self
            .pool
            .conn()
            .execute(
                "INSERT OR IGNORE INTO cron_executions (id, job_id, user_id, scheduled_fire_time, triggered_at, status, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                libsql::params![
                    exec.id.clone(),
                    exec.job_id.clone(),
                    exec.user_id.clone(),
                    scheduled_us,
                    triggered_us,
                    execution_status_str(exec.status).to_string(),
                    data,
                ],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql insert: {e}")))?;
        if affected == 0 {
            return Err(CronError::AlreadyDispatched(format!(
                "{}@{}",
                exec.job_id, scheduled_us
            )));
        }
        Ok(())
    }

    async fn list_executions_by_job(&self, job_id: &str) -> Result<Vec<CronExecution>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE job_id = ?1 ORDER BY triggered_at DESC",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            out.push(execution_from_row(&row)?);
        }
        Ok(out)
    }

    async fn list_executions_by_user(&self, user_id: &str) -> Result<Vec<CronExecution>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE user_id = ?1 ORDER BY triggered_at DESC",
                libsql::params![user_id.to_string()],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            out.push(execution_from_row(&row)?);
        }
        Ok(out)
    }

    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time_us: i64,
    ) -> Result<bool> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT 1 FROM cron_executions WHERE job_id = ?1 AND scheduled_fire_time = ?2 LIMIT 1",
                libsql::params![job_id.to_string(), scheduled_fire_time_us],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let exists = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
            .is_some();
        Ok(exists)
    }

    async fn update_execution_status(
        &self,
        execution_id: &str,
        status: ExecutionStatus,
    ) -> Result<()> {
        let affected = self
            .pool
            .conn()
            .execute(
                "UPDATE cron_executions SET status = ?1 WHERE id = ?2",
                libsql::params![
                    execution_status_str(status).to_string(),
                    execution_id.to_string(),
                ],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql update: {e}")))?;

        if affected == 0 {
            return Err(CronError::NotFound(execution_id.to_string()));
        }
        Ok(())
    }

    async fn list_executions_by_status(
        &self,
        status: ExecutionStatus,
    ) -> Result<Vec<CronExecution>> {
        let mut rows = self
            .pool
            .conn()
            .query(
                "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE status = ?1",
                libsql::params![execution_status_str(status).to_string()],
            )
            .await
            .map_err(|e| CronError::Storage(format!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CronError::Storage(format!("libsql row: {e}")))?
        {
            out.push(execution_from_row(&row)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_cron::{CronSchedule, CronStatus};
    use aura_model::ChannelType;
    use chrono::Utc;

    fn future_dt() -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn test_job(id: &str, user_id: &str, status: CronStatus) -> CronJob {
        let now = Utc::now();
        CronJob {
            id: id.to_string(),
            user_id: user_id.to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::cron("0 9 * * *"),
            prompt: "test".to_string(),
            timezone: "UTC".to_string(),
            status,
            last_triggered_at: None,
            next_trigger_at: Some(future_dt()),
            created_at: now,
            updated_at: now,
            origin_session_id: None,
        }
    }

    fn test_execution(id: &str, job_id: &str, user_id: &str) -> CronExecution {
        // Salt the schedule slot with the id's hash so the
        // UNIQUE(job_id, scheduled_fire_time) constraint isn't tripped
        // by repeat fixture rows.
        let salt: i64 = id.chars().map(|c| c as i64).sum();
        let scheduled =
            future_dt() + chrono::Duration::microseconds(salt);
        CronExecution {
            id: id.to_string(),
            job_id: job_id.to_string(),
            user_id: user_id.to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::cron("0 9 * * *"),
            prompt: "test".to_string(),
            scheduled_fire_time: scheduled,
            triggered_at: future_dt(),
            status: ExecutionStatus::Pending,
            origin_session_id: None,
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        store
            .create(&test_job("cj-1", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        let loaded = store.get("cj-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "cj-1");
        assert_eq!(loaded.user_id, "u1");
    }

    #[tokio::test]
    async fn save_updates_row() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        let mut job = test_job("cj-2", "u1", CronStatus::Enabled);
        store.create(&job).await.unwrap();

        job.status = CronStatus::Disabled;
        store.save(&job).await.unwrap();

        let loaded = store.get("cj-2").await.unwrap().unwrap();
        assert_eq!(loaded.status, CronStatus::Disabled);
    }

    #[tokio::test]
    async fn save_nonexistent_returns_not_found() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        let err = store
            .save(&test_job("nonexistent", "u1", CronStatus::Enabled))
            .await
            .unwrap_err();
        assert!(matches!(err, CronError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        store
            .create(&test_job("cj-3", "u1", CronStatus::Enabled))
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
        assert!(matches!(err, CronError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_by_user_filters_correctly() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);
        store
            .create(&test_job("cj-4", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        store
            .create(&test_job("cj-5", "u2", CronStatus::Enabled))
            .await
            .unwrap();
        store
            .create(&test_job("cj-6", "u1", CronStatus::Disabled))
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
            .create(&test_job("cj-7", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        store
            .create(&test_job("cj-8", "u1", CronStatus::Disabled))
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

        // 2000-01-01 / 2025-01-01.
        let past = chrono::DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let now_us = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            .timestamp_micros();

        let mut past_due = test_job("cj-9", "u1", CronStatus::Enabled);
        past_due.next_trigger_at = Some(past);
        store.create(&past_due).await.unwrap();

        let future = test_job("cj-10", "u1", CronStatus::Enabled);
        store.create(&future).await.unwrap();

        let mut disabled = test_job("cj-11", "u1", CronStatus::Disabled);
        disabled.next_trigger_at = Some(past);
        store.create(&disabled).await.unwrap();

        let due = store.list_due(now_us).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "cj-9");
    }

    // ── Execution record tests ──

    #[tokio::test]
    async fn record_execution_dedup_returns_already_dispatched() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);

        let mut exec = test_execution("ce-dup-a", "cj-dup", "u1");
        store.record_execution(&exec).await.unwrap();

        // Second instance of the same scheduler racing onto the same
        // (job_id, scheduled_fire_time) slot — different `id`, same dedup key.
        exec.id = "ce-dup-b".into();
        let err = store.record_execution(&exec).await.unwrap_err();
        match err {
            CronError::AlreadyDispatched(key) => {
                assert!(key.starts_with("cj-dup@"), "key was {key}");
            }
            other => panic!("expected AlreadyDispatched, got {other:?}"),
        }

        // Original row remains; the loser produced no orphan.
        let execs = store.list_executions_by_job("cj-dup").await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "ce-dup-a");
    }

    #[tokio::test]
    async fn record_and_list_executions_by_job() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCronStore::new(pool);

        store
            .record_execution(&test_execution("ce-1", "cj-1", "u1"))
            .await
            .unwrap();
        store
            .record_execution(&test_execution("ce-2", "cj-1", "u1"))
            .await
            .unwrap();
        store
            .record_execution(&test_execution("ce-3", "cj-2", "u1"))
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
            .record_execution(&test_execution("ce-4", "cj-1", "u1"))
            .await
            .unwrap();
        store
            .record_execution(&test_execution("ce-5", "cj-2", "u2"))
            .await
            .unwrap();
        store
            .record_execution(&test_execution("ce-6", "cj-3", "u1"))
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
            .create(&test_job("cj-evict", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        store
            .record_execution(&test_execution("ce-7", "cj-evict", "u1"))
            .await
            .unwrap();

        store.delete("cj-evict").await.unwrap();
        assert!(store.get("cj-evict").await.unwrap().is_none());

        let execs = store.list_executions_by_job("cj-evict").await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "ce-7");
    }
}
