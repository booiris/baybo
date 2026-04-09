use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use crate::cost::{CostError, CostRecord, CostResult, CostStore, CostSummary, TimeRange};

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
            "INSERT INTO cost_records (user_id, session_id, job_id, trace_span_id, model, input_tokens, output_tokens, cost_usd, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            libsql::params![
                record.user_id.clone(),
                record.session_id.clone(),
                record.job_id.clone(),
                record.trace_span_id.clone(),
                record.model.clone(),
                record.input_tokens as i64,
                record.output_tokens as i64,
                record.cost_usd,
                record.timestamp.to_rfc3339(),
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
                "SELECT user_id, session_id, job_id, trace_span_id, model, input_tokens, output_tokens, cost_usd, timestamp FROM cost_records WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                libsql::params![
                    user_id.to_string(),
                    range.from.to_rfc3339(),
                    range.to.to_rfc3339(),
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
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), COUNT(*) FROM cost_records WHERE timestamp >= ?1 AND timestamp < ?2",
                libsql::params![range.from.to_rfc3339(), range.to.to_rfc3339()],
            )
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
            .ok_or_else(|| CostError::Storage("expected aggregate row".to_string()))?;

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

    async fn sum_user(&self, user_id: &str, range: TimeRange) -> CostResult<f64> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                libsql::params![
                    user_id.to_string(),
                    range.from.to_rfc3339(),
                    range.to.to_rfc3339(),
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
}

fn row_to_cost_record(row: &libsql::Row) -> CostResult<CostRecord> {
    let timestamp_str: String = row
        .get(8)
        .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?;
    let timestamp: DateTime<Utc> = timestamp_str
        .parse()
        .map_err(|e| CostError::Storage(format!("invalid timestamp: {e}")))?;

    Ok(CostRecord {
        user_id: row
            .get(0)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        session_id: row
            .get(1)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        job_id: row
            .get(2)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        trace_span_id: row
            .get(3)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn test_record(user_id: &str, cost: f64) -> CostRecord {
        CostRecord {
            user_id: user_id.to_string(),
            session_id: "sess-1".to_string(),
            job_id: "job-1".to_string(),
            trace_span_id: "span-1".to_string(),
            model: "gpt-4".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: cost,
            timestamp: Utc::now(),
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
