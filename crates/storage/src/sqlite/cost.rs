use async_trait::async_trait;
use aura_core::AuraError;
use aura_cost::{CostRecord, CostStore, CostSummary, TimeRange};
use chrono::{DateTime, Utc};

use super::SqlitePool;

pub struct SqliteCostStore {
    pool: SqlitePool,
}

impl SqliteCostStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CostStore for SqliteCostStore {
    async fn record(&self, record: &CostRecord) -> aura_core::Result<()> {
        let pool = self.pool.clone();
        let user_id = record.user_id.clone();
        let session_id = record.session_id.clone();
        let job_id = record.job_id.clone();
        let trace_span_id = record.trace_span_id.clone();
        let model = record.model.clone();
        let input_tokens = record.input_tokens as i64;
        let output_tokens = record.output_tokens as i64;
        let cost_usd = record.cost_usd;
        let timestamp = record.timestamp.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "INSERT INTO cost_records (user_id, session_id, job_id, trace_span_id, model, input_tokens, output_tokens, cost_usd, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![user_id, session_id, job_id, trace_span_id, model, input_tokens, output_tokens, cost_usd, timestamp],
            )
            .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite insert error: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn query_user(
        &self,
        user_id: &str,
        range: TimeRange,
    ) -> aura_core::Result<Vec<CostRecord>> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        let from = range.from.to_rfc3339();
        let to = range.to.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT user_id, session_id, job_id, trace_span_id, model, input_tokens, output_tokens, cost_usd, timestamp FROM cost_records WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                )
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![user_id, from, to], |row| {
                    Ok(CostRecordRow {
                        user_id: row.get(0)?,
                        session_id: row.get(1)?,
                        job_id: row.get(2)?,
                        trace_span_id: row.get(3)?,
                        model: row.get(4)?,
                        input_tokens: row.get::<_, i64>(5)?,
                        output_tokens: row.get::<_, i64>(6)?,
                        cost_usd: row.get(7)?,
                        timestamp: row.get::<_, String>(8)?,
                    })
                })
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;

            let mut records = Vec::new();
            for row in rows {
                let r = row.map_err(|e| {
                    AuraError::Internal(anyhow::anyhow!("sqlite row error: {e}"))
                })?;
                records.push(r.into_cost_record()?);
            }
            Ok(records)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn query_global(&self, range: TimeRange) -> aura_core::Result<CostSummary> {
        let pool = self.pool.clone();
        let from = range.from.to_rfc3339();
        let to = range.to.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), COUNT(*) FROM cost_records WHERE timestamp >= ?1 AND timestamp < ?2",
                )
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let summary = stmt
                .query_row(rusqlite::params![from, to], |row| {
                    Ok(CostSummary {
                        total_cost_usd: row.get(0)?,
                        total_input_tokens: row.get::<_, i64>(1)? as usize,
                        total_output_tokens: row.get::<_, i64>(2)? as usize,
                        record_count: row.get::<_, i64>(3)? as usize,
                    })
                })
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            Ok(summary)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn sum_user(&self, user_id: &str, range: TimeRange) -> aura_core::Result<f64> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        let from = range.from.to_rfc3339();
        let to = range.to.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                )
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let sum: f64 = stmt
                .query_row(rusqlite::params![user_id, from, to], |row| row.get(0))
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            Ok(sum)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }
}

/// Intermediate struct for reading cost records from SQLite rows.
struct CostRecordRow {
    user_id: String,
    session_id: String,
    job_id: String,
    trace_span_id: String,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cost_usd: f64,
    timestamp: String,
}

impl CostRecordRow {
    fn into_cost_record(self) -> aura_core::Result<CostRecord> {
        let timestamp: DateTime<Utc> = self
            .timestamp
            .parse()
            .map_err(|e| AuraError::Serialization(format!("invalid timestamp: {e}")))?;
        Ok(CostRecord {
            user_id: self.user_id,
            session_id: self.session_id,
            job_id: self.job_id,
            trace_span_id: self.trace_span_id,
            model: self.model,
            input_tokens: self.input_tokens as usize,
            output_tokens: self.output_tokens as usize,
            cost_usd: self.cost_usd,
            timestamp,
        })
    }
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
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteCostStore::new(pool);
        store.record(&test_record("u1", 0.05)).await.unwrap();
        let records = store.query_user("u1", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert!((records[0].cost_usd - 0.05).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn query_global_summary() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteCostStore::new(pool);
        store.record(&test_record("u1", 0.10)).await.unwrap();
        store.record(&test_record("u2", 0.20)).await.unwrap();
        let summary = store.query_global(wide_range()).await.unwrap();
        assert_eq!(summary.record_count, 2);
        assert!((summary.total_cost_usd - 0.30).abs() < 0.001);
    }

    #[tokio::test]
    async fn sum_user_cost() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteCostStore::new(pool);
        store.record(&test_record("u1", 0.10)).await.unwrap();
        store.record(&test_record("u1", 0.25)).await.unwrap();
        store.record(&test_record("u2", 1.00)).await.unwrap();
        let sum = store.sum_user("u1", wide_range()).await.unwrap();
        assert!((sum - 0.35).abs() < 0.001);
    }
}
