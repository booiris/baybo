use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::SqlitePool;
use super::time;
use baybo_model::{CallReason, CostRecord, CostSummary, TimeRange};
use baybo_model::{JobId, MicroUsd, SessionId, SpanId};
use baybo_store::StorageError;
use baybo_store::cost::{CostStore, Result as CostResult};

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
    async fn record(&self, record: &CostRecord) -> CostResult<()> {
        let user_id = record.user_id.clone();
        let session_id = record.session_id.as_str().to_string();
        let job_id = record.job_id.to_string();
        let span_id = record.span_id.to_string();
        let reason = record.reason.to_token().into_owned();
        let model = record.model.clone();
        let input_tokens = record.input_tokens as i64;
        let output_tokens = record.output_tokens as i64;
        let cached_input_tokens = record.cached_input_tokens as i64;
        let cache_creation_input_tokens = record.cache_creation_input_tokens as i64;
        let cost_usd = record.cost_usd.into_micros();
        let timestamp = time::to_us(record.timestamp);
        self.pool
            .interact("cost.record", move |conn| {
                conn.execute(
                    "INSERT INTO cost_records \
                     (user_id, session_id, job_id, span_id, reason, model, input_tokens, output_tokens, \
                      cached_input_tokens, cache_creation_input_tokens, cost_usd, timestamp) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        user_id,
                        session_id,
                        job_id,
                        span_id,
                        reason,
                        model,
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        cache_creation_input_tokens,
                        cost_usd,
                        timestamp,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn query_user(&self, user_id: &str, range: TimeRange) -> CostResult<Vec<CostRecord>> {
        let user_id = user_id.to_string();
        let from = time::to_us(range.from);
        let to = time::to_us(range.to);
        let raws = self
            .pool
            .interact("cost.query_user", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT user_id, session_id, job_id, span_id, reason, model, input_tokens, \
                            output_tokens, cached_input_tokens, cache_creation_input_tokens, \
                            cost_usd, timestamp \
                     FROM cost_records \
                     WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                )?;
                let raws = stmt
                    .query_map(rusqlite::params![user_id, from, to], raw_cost_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(raw_to_cost_record).collect()
    }

    async fn query_global(&self, range: TimeRange) -> CostResult<CostSummary> {
        let from = time::to_us(range.from);
        let to = time::to_us(range.to);
        let summary = self
            .pool
            .interact("cost.query_global", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                                COALESCE(SUM(output_tokens), 0), \
                                COALESCE(SUM(cached_input_tokens), 0), \
                                COALESCE(SUM(cache_creation_input_tokens), 0), \
                                COUNT(*) \
                         FROM cost_records WHERE timestamp >= ?1 AND timestamp < ?2",
                        rusqlite::params![from, to],
                        summary_from_aggregate_row,
                    )
                    .optional()?)
            })
            .await?;
        summary.ok_or_else(|| StorageError::Storage("expected aggregate row".to_string()))
    }

    async fn query_session(&self, session_id: &SessionId) -> CostResult<CostSummary> {
        let session_id = session_id.as_str().to_string();
        let summary = self
            .pool
            .interact("cost.query_session", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                                COALESCE(SUM(output_tokens), 0), \
                                COALESCE(SUM(cached_input_tokens), 0), \
                                COALESCE(SUM(cache_creation_input_tokens), 0), \
                                COUNT(*) \
                         FROM cost_records WHERE session_id = ?1",
                        rusqlite::params![session_id],
                        summary_from_aggregate_row,
                    )
                    .optional()?)
            })
            .await?;
        summary.ok_or_else(|| StorageError::Storage("expected aggregate row".to_string()))
    }

    async fn query_records_in_range(&self, range: TimeRange) -> CostResult<Vec<CostRecord>> {
        let from = time::to_us(range.from);
        let to = time::to_us(range.to);
        let raws = self
            .pool
            .interact("cost.query_records_in_range", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT user_id, session_id, job_id, span_id, reason, model, input_tokens, \
                            output_tokens, cached_input_tokens, cache_creation_input_tokens, \
                            cost_usd, timestamp \
                     FROM cost_records \
                     WHERE timestamp >= ?1 AND timestamp < ?2 \
                     ORDER BY timestamp",
                )?;
                let raws = stmt
                    .query_map(rusqlite::params![from, to], raw_cost_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(raw_to_cost_record).collect()
    }

    async fn query_job(&self, job_id: &JobId) -> CostResult<CostSummary> {
        let job_id = job_id.to_string();
        let summary = self
            .pool
            .interact("cost.query_job", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                                COALESCE(SUM(output_tokens), 0), \
                                COALESCE(SUM(cached_input_tokens), 0), \
                                COALESCE(SUM(cache_creation_input_tokens), 0), \
                                COUNT(*) \
                         FROM cost_records WHERE job_id = ?1",
                        rusqlite::params![job_id],
                        summary_from_aggregate_row,
                    )
                    .optional()?)
            })
            .await?;
        summary.ok_or_else(|| StorageError::Storage("expected aggregate row".to_string()))
    }
}

fn summary_from_aggregate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CostSummary> {
    Ok(CostSummary {
        total_cost_usd: MicroUsd::from_micros(row.get::<_, i64>(0)?),
        total_input_tokens: row.get::<_, i64>(1)? as usize,
        total_output_tokens: row.get::<_, i64>(2)? as usize,
        total_cached_input_tokens: row.get::<_, i64>(3)? as usize,
        total_cache_creation_input_tokens: row.get::<_, i64>(4)? as usize,
        record_count: row.get::<_, i64>(5)? as usize,
    })
}

/// Raw column values of one `cost_records` row.
///
/// The `query_map` row closure can only fail with a `rusqlite::Error`, so the
/// columns that still need fallible decoding (ids, timestamp) are carried out
/// verbatim and turned into a [`CostRecord`] by [`raw_to_cost_record`].
struct RawCostRow {
    user_id: String,
    session_id: String,
    job_id: String,
    span_id: String,
    reason: Option<String>,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    cache_creation_input_tokens: i64,
    cost_usd: i64,
    timestamp_us: i64,
}

fn raw_cost_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawCostRow> {
    Ok(RawCostRow {
        user_id: row.get(0)?,
        session_id: row.get(1)?,
        job_id: row.get(2)?,
        span_id: row.get(3)?,
        reason: row.get(4)?,
        model: row.get(5)?,
        input_tokens: row.get(6)?,
        output_tokens: row.get(7)?,
        cached_input_tokens: row.get(8)?,
        cache_creation_input_tokens: row.get(9)?,
        cost_usd: row.get(10)?,
        timestamp_us: row.get(11)?,
    })
}

fn raw_to_cost_record(raw: RawCostRow) -> CostResult<CostRecord> {
    let timestamp_us = raw.timestamp_us;
    let timestamp = time::from_us(timestamp_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "cost_records.timestamp out of range: {timestamp_us}"
        ))
    })?;

    // Rows written before the `reason` column exists read NULL; unknown
    // tokens (a future variant rolled back) likewise fall back to the
    // default reason rather than failing the whole query.
    let reason = raw
        .reason
        .as_deref()
        .and_then(CallReason::parse)
        .unwrap_or_default();

    Ok(CostRecord {
        user_id: raw.user_id,
        session_id: SessionId::from(raw.session_id),
        job_id: raw
            .job_id
            .parse::<JobId>()
            .map_err(|e| StorageError::Storage(format!("decode job_id: {e}")))?,
        span_id: raw
            .span_id
            .parse::<SpanId>()
            .map_err(|e| StorageError::Storage(format!("decode span_id: {e}")))?,
        reason,
        model: raw.model,
        input_tokens: raw.input_tokens as usize,
        output_tokens: raw.output_tokens as usize,
        cached_input_tokens: raw.cached_input_tokens as usize,
        cache_creation_input_tokens: raw.cache_creation_input_tokens as usize,
        cost_usd: MicroUsd::from_micros(raw.cost_usd),
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
            reason: CallReason::Chat,
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteCostStore::new(pool);
        store.record(&test_record("u1", usd(0.05))).await.unwrap();
        let records = store.query_user("u1", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cost_usd, usd(0.05));
    }

    #[tokio::test]
    async fn query_global_summary() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteCostStore::new(pool);
        store.record(&test_record("u1", usd(0.10))).await.unwrap();
        store.record(&test_record("u2", usd(0.20))).await.unwrap();
        let summary = store.query_global(wide_range()).await.unwrap();
        assert_eq!(summary.record_count, 2);
        assert_eq!(summary.total_cost_usd, usd(0.30));
    }

    #[tokio::test]
    async fn reason_round_trips() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteCostStore::new(pool);
        let mut rec = test_record("u1", usd(0.05));
        rec.reason = CallReason::ProgressObserver;
        store.record(&rec).await.unwrap();
        let records = store.query_user("u1", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reason, CallReason::ProgressObserver);
    }

    /// The `Tool(name)` variant must survive the `tool:<name>` token
    /// round-trip through the column, name intact.
    #[tokio::test]
    async fn tool_reason_round_trips_with_name() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteCostStore::new(pool);
        let mut rec = test_record("u2", usd(0.05));
        rec.reason = CallReason::Tool("WebFetch".to_string());
        store.record(&rec).await.unwrap();
        let records = store.query_user("u2", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reason, CallReason::Tool("WebFetch".to_string()));
    }
}
