use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use super::time;
use crate::cost::{
    CostError, CostRecord, CostResult, CostStore, CostSummary, TimeRange, UserMonthlyCost,
};
use aura_model::{JobId, SessionId, SpanId};

pub struct LibsqlCostStore {
    pool: LibsqlPool,
}

impl LibsqlCostStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CostStore for LibsqlCostStore {
    async fn record(&self, record: &CostRecord) -> CostResult<()> {
        let conn = self.pool.conn();
        conn.execute(
            "INSERT INTO cost_records \
             (user_id, session_id, job_id, span_id, model, input_tokens, output_tokens, \
              cost_usd, timestamp, originating_session_deleted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            libsql::params![
                record.user_id.clone(),
                record.session_id.as_str().to_string(),
                record.job_id.to_string(),
                record.span_id.to_string(),
                record.model.clone(),
                record.input_tokens as i64,
                record.output_tokens as i64,
                record.cost_usd,
                time::to_us(record.timestamp),
                record.originating_session_deleted_at.map(time::to_us),
            ],
        )
        .await
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql insert error: {e}")))?;
        Ok(())
    }

    async fn query_user(&self, user_id: &str, range: TimeRange) -> CostResult<Vec<CostRecord>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT user_id, session_id, job_id, span_id, model, input_tokens, \
                        output_tokens, cost_usd, timestamp, originating_session_deleted_at \
                 FROM cost_records \
                 WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                libsql::params![
                    user_id.to_string(),
                    time::to_us(range.from),
                    time::to_us(range.to),
                ],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut records = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            records.push(row_to_cost_record(&row)?);
        }
        Ok(records)
    }

    async fn query_global(&self, range: TimeRange) -> CostResult<CostSummary> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                        COALESCE(SUM(output_tokens), 0), COUNT(*) \
                 FROM cost_records WHERE timestamp >= ?1 AND timestamp < ?2",
                libsql::params![time::to_us(range.from), time::to_us(range.to)],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;
        let row = rows
            .next()
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
            .ok_or_else(|| CostError::Storage("expected aggregate row".to_string()))?;
        summary_from_aggregate_row(&row)
    }

    async fn query_session(&self, session_id: &SessionId) -> CostResult<CostSummary> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                        COALESCE(SUM(output_tokens), 0), COUNT(*) \
                 FROM cost_records WHERE session_id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;
        let row = rows
            .next()
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
            .ok_or_else(|| CostError::Storage("expected aggregate row".to_string()))?;
        summary_from_aggregate_row(&row)
    }

    async fn query_job(&self, job_id: &JobId) -> CostResult<CostSummary> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                        COALESCE(SUM(output_tokens), 0), COUNT(*) \
                 FROM cost_records WHERE job_id = ?1",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;
        let row = rows
            .next()
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
            .ok_or_else(|| CostError::Storage("expected aggregate row".to_string()))?;
        summary_from_aggregate_row(&row)
    }

    async fn sum_user(&self, user_id: &str, range: TimeRange) -> CostResult<f64> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records \
                 WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                libsql::params![
                    user_id.to_string(),
                    time::to_us(range.from),
                    time::to_us(range.to),
                ],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
            .ok_or_else(|| CostError::Storage("expected aggregate row".to_string()))?;

        row.get::<f64>(0)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))
    }

    async fn bump_user_monthly_cost(
        &self,
        user_id: &str,
        month: &str,
        delta_usd: f64,
    ) -> CostResult<()> {
        let conn = self.pool.conn();
        let now = time::now_us();
        // ON CONFLICT DO UPDATE so a re-bump increments the running
        // total instead of clobbering it; resetting deleted_at to NULL
        // revives a soft-deleted row (matches the storage-wide
        // soft-delete protocol).
        conn.execute(
            "INSERT INTO user_monthly_cost (user_id, month, cost_usd, updated_at, deleted_at) \
             VALUES (?1, ?2, ?3, ?4, NULL) \
             ON CONFLICT(user_id, month) DO UPDATE SET \
               cost_usd = cost_usd + excluded.cost_usd, \
               updated_at = excluded.updated_at, \
               deleted_at = NULL",
            libsql::params![user_id.to_string(), month.to_string(), delta_usd, now],
        )
        .await
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql upsert: {e}")))?;
        Ok(())
    }

    async fn get_user_monthly_cost(
        &self,
        user_id: &str,
        month: &str,
    ) -> CostResult<Option<UserMonthlyCost>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT user_id, month, cost_usd, updated_at FROM user_monthly_cost \
                 WHERE user_id = ?1 AND month = ?2 AND deleted_at IS NULL",
                libsql::params![user_id.to_string(), month.to_string()],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        let row = rows
            .next()
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql row: {e}")))?;
        match row {
            None => Ok(None),
            Some(row) => {
                let user_id: String = row
                    .get(0)
                    .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let month: String = row
                    .get(1)
                    .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let cost_usd: f64 = row
                    .get(2)
                    .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let updated_at: i64 = row
                    .get(3)
                    .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                Ok(Some(UserMonthlyCost {
                    user_id,
                    month,
                    cost_usd,
                    updated_at: time::from_us(updated_at).ok_or_else(|| {
                        CostError::Storage(format!(
                            "user_monthly_cost.updated_at out of range: {updated_at}"
                        ))
                    })?,
                }))
            }
        }
    }

    async fn purge_user_monthly_cost_older_than(&self, cutoff: DateTime<Utc>) -> CostResult<u64> {
        let conn = self.pool.conn();
        let now = time::now_us();
        let affected = conn
            .execute(
                "UPDATE user_monthly_cost SET deleted_at = ?1 \
                 WHERE updated_at < ?2 AND deleted_at IS NULL",
                libsql::params![now, time::to_us(cutoff)],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql update: {e}")))?;
        Ok(affected)
    }

    async fn purge_cost_records_older_than(&self, cutoff: DateTime<Utc>) -> CostResult<u64> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "DELETE FROM cost_records WHERE timestamp < ?1",
                libsql::params![time::to_us(cutoff)],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql delete: {e}")))?;
        Ok(affected)
    }
}

fn summary_from_aggregate_row(row: &libsql::Row) -> CostResult<CostSummary> {
    Ok(CostSummary {
        total_cost_usd: row
            .get::<f64>(0)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        total_input_tokens: row
            .get::<i64>(1)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        total_output_tokens: row
            .get::<i64>(2)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        record_count: row
            .get::<i64>(3)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
    })
}

fn row_to_cost_record(row: &libsql::Row) -> CostResult<CostRecord> {
    let timestamp_us: i64 = row
        .get(8)
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?;
    let timestamp = time::from_us(timestamp_us).ok_or_else(|| {
        CostError::Storage(format!(
            "cost_records.timestamp out of range: {timestamp_us}"
        ))
    })?;

    let session_id_str: String = row
        .get(1)
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?;
    let job_id_str: String = row
        .get(2)
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?;
    let span_id_str: String = row
        .get(3)
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?;

    let originating_deleted_ts: Option<i64> = row
        .get(9)
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?;
    let originating_session_deleted_at = match originating_deleted_ts {
        None => None,
        Some(us) => Some(time::from_us(us).ok_or_else(|| {
            CostError::Storage(format!(
                "cost_records.originating_session_deleted_at out of range: {us}"
            ))
        })?),
    };

    Ok(CostRecord {
        user_id: row
            .get(0)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        session_id: SessionId::from(session_id_str),
        job_id: job_id_str
            .parse::<JobId>()
            .map_err(|e| CostError::Storage(format!("decode job_id: {e}")))?,
        span_id: span_id_str
            .parse::<SpanId>()
            .map_err(|e| CostError::Storage(format!("decode span_id: {e}")))?,
        model: row
            .get(4)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        input_tokens: row
            .get::<i64>(5)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        output_tokens: row
            .get::<i64>(6)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        cost_usd: row
            .get(7)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        timestamp,
        originating_session_deleted_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn test_record(user_id: &str, cost: f64) -> CostRecord {
        CostRecord {
            user_id: user_id.to_string(),
            session_id: SessionId::from("sess-1"),
            job_id: JobId::new(),
            span_id: SpanId::new(),
            model: "gpt-4".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: cost,
            timestamp: Utc::now(),
            originating_session_deleted_at: None,
        }
    }

    fn wide_range() -> TimeRange {
        TimeRange {
            from: Utc::now() - Duration::hours(1),
            to: Utc::now() + Duration::hours(1),
        }
    }

    #[tokio::test]
    async fn record_and_query() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCostStore::new(pool);
        store.record(&test_record("u1", 0.05)).await.unwrap();
        let records = store.query_user("u1", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert!((records[0].cost_usd - 0.05).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn query_global_summary() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCostStore::new(pool);
        store.record(&test_record("u1", 0.10)).await.unwrap();
        store.record(&test_record("u2", 0.20)).await.unwrap();
        let summary = store.query_global(wide_range()).await.unwrap();
        assert_eq!(summary.record_count, 2);
        assert!((summary.total_cost_usd - 0.30).abs() < 0.001);
    }

    #[tokio::test]
    async fn sum_user_cost() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCostStore::new(pool);
        store.record(&test_record("u1", 0.10)).await.unwrap();
        store.record(&test_record("u1", 0.25)).await.unwrap();
        store.record(&test_record("u2", 1.00)).await.unwrap();
        let sum = store.sum_user("u1", wide_range()).await.unwrap();
        assert!((sum - 0.35).abs() < 0.001);
    }
}
