use async_trait::async_trait;

use super::LibsqlPool;
use super::time;
use crate::cost::{CostError, CostRecord, CostResult, CostStore, CostSummary, TimeRange};
use aura_model::{JobId, MicroUsd, SessionId, SpanId};

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
              cached_input_tokens, cache_creation_input_tokens, cost_usd, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            libsql::params![
                record.user_id.clone(),
                record.session_id.as_str().to_string(),
                record.job_id.to_string(),
                record.span_id.to_string(),
                record.model.clone(),
                record.input_tokens as i64,
                record.output_tokens as i64,
                record.cached_input_tokens as i64,
                record.cache_creation_input_tokens as i64,
                record.cost_usd.into_micros(),
                time::to_us(record.timestamp),
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
                        output_tokens, cached_input_tokens, cache_creation_input_tokens, \
                        cost_usd, timestamp \
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
                        COALESCE(SUM(output_tokens), 0), \
                        COALESCE(SUM(cached_input_tokens), 0), \
                        COALESCE(SUM(cache_creation_input_tokens), 0), \
                        COUNT(*) \
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
                        COALESCE(SUM(output_tokens), 0), \
                        COALESCE(SUM(cached_input_tokens), 0), \
                        COALESCE(SUM(cache_creation_input_tokens), 0), \
                        COUNT(*) \
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

    async fn query_records_in_range(&self, range: TimeRange) -> CostResult<Vec<CostRecord>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT user_id, session_id, job_id, span_id, model, input_tokens, \
                        output_tokens, cached_input_tokens, cache_creation_input_tokens, \
                        cost_usd, timestamp \
                 FROM cost_records \
                 WHERE timestamp >= ?1 AND timestamp < ?2 \
                 ORDER BY timestamp",
                libsql::params![time::to_us(range.from), time::to_us(range.to)],
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

    async fn query_job(&self, job_id: &JobId) -> CostResult<CostSummary> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                        COALESCE(SUM(output_tokens), 0), \
                        COALESCE(SUM(cached_input_tokens), 0), \
                        COALESCE(SUM(cache_creation_input_tokens), 0), \
                        COUNT(*) \
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
}

fn summary_from_aggregate_row(row: &libsql::Row) -> CostResult<CostSummary> {
    Ok(CostSummary {
        total_cost_usd: row
            .get::<i64>(0)
            .map(MicroUsd::from_micros)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        total_input_tokens: row
            .get::<i64>(1)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        total_output_tokens: row
            .get::<i64>(2)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        total_cached_input_tokens: row
            .get::<i64>(3)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        total_cache_creation_input_tokens: row
            .get::<i64>(4)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        record_count: row
            .get::<i64>(5)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
    })
}

fn row_to_cost_record(row: &libsql::Row) -> CostResult<CostRecord> {
    let timestamp_us: i64 = row
        .get(10)
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
        cached_input_tokens: row
            .get::<i64>(7)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        cache_creation_input_tokens: row
            .get::<i64>(8)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?
            as usize,
        cost_usd: row
            .get::<i64>(9)
            .map(MicroUsd::from_micros)
            .map_err(|e| CostError::Internal(anyhow::anyhow!("libsql get error: {e}")))?,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn test_record(user_id: &str, cost: MicroUsd) -> CostRecord {
        CostRecord {
            user_id: user_id.to_string(),
            session_id: SessionId::from("sess-1"),
            job_id: JobId::new(),
            span_id: SpanId::new(),
            model: "gpt-4".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cost_usd: cost,
            timestamp: Utc::now(),
        }
    }

    fn usd(v: f64) -> MicroUsd {
        MicroUsd::from_usd_decimal(v)
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
        store.record(&test_record("u1", usd(0.05))).await.unwrap();
        let records = store.query_user("u1", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cost_usd, usd(0.05));
    }

    #[tokio::test]
    async fn query_global_summary() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlCostStore::new(pool);
        store.record(&test_record("u1", usd(0.10))).await.unwrap();
        store.record(&test_record("u2", usd(0.20))).await.unwrap();
        let summary = store.query_global(wide_range()).await.unwrap();
        assert_eq!(summary.record_count, 2);
        assert_eq!(summary.total_cost_usd, usd(0.30));
    }
}
