use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::time;
use super::{SqlitePool, row::RowDecoder};
use baybo_model::{CallReason, CostRecord, CostSummary, TimeRange};
use baybo_model::{MicroUsd, SessionId, SpanId, TurnId};
use baybo_store::StorageError;
use baybo_store::cost::{CostGroupBucket, CostGroupKey, CostStore, Result as CostResult};

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
        let turn_id = record.turn_id.to_string();
        let span_id = record.span_id.to_string();
        let reason = record.reason.to_token().into_owned();
        let model = record.model.clone();
        let reasoning_effort = record.reasoning_effort.clone();
        let input_tokens = record.input_tokens as i64;
        let output_tokens = record.output_tokens as i64;
        let cached_input_tokens = record.cached_input_tokens as i64;
        let cache_creation_input_tokens = record.cache_creation_input_tokens as i64;
        let cost_usd = record.cost_usd.into_micros();
        let timestamp = time::to_us(record.timestamp);
        self.pool
            .interact_write("cost.record", move |conn| {
                conn.execute(
                    "INSERT INTO cost_records \
                     (user_id, session_id, turn_id, span_id, reason, model, reasoning_effort, \
                      input_tokens, output_tokens, cached_input_tokens, \
                      cache_creation_input_tokens, cost_usd, timestamp) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    rusqlite::params![
                        user_id,
                        session_id,
                        turn_id,
                        span_id,
                        reason,
                        model,
                        reasoning_effort,
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
        self
            .pool
            .interact("cost.query_user", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, session_id, turn_id, span_id, reason, model, reasoning_effort, \
                            input_tokens, output_tokens, cached_input_tokens, \
                            cache_creation_input_tokens, cost_usd, timestamp \
                     FROM cost_records \
                     WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
                )?;
                COST.list(stmt.query(rusqlite::params![user_id, from, to])?)
            })
            .await
    }

    async fn query_user_summary(&self, user_id: &str, range: TimeRange) -> CostResult<CostSummary> {
        let user_id = user_id.to_string();
        let from = time::to_us(range.from);
        let to = time::to_us(range.to);
        let summary = self
            .pool
            .interact("cost.query_user_summary", move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {SUMMARY_COLUMNS} \
                         FROM cost_records \
                         WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3"
                        ),
                        rusqlite::params![user_id, from, to],
                        summary_from_aggregate_row,
                    )
                    .optional()?)
            })
            .await?;
        summary.ok_or_else(|| StorageError::Storage("expected aggregate row".to_string()))
    }

    async fn query_range_grouped(
        &self,
        range: TimeRange,
        key: CostGroupKey,
    ) -> CostResult<Vec<CostGroupBucket>> {
        let from = time::to_us(range.from);
        let to = time::to_us(range.to);
        // `timestamp` is Unix µs; strftime wants seconds.
        let key_expr = match key {
            CostGroupKey::Day => "strftime('%Y-%m-%d', timestamp / 1000000, 'unixepoch')",
            CostGroupKey::Model => "model",
            CostGroupKey::Reason => "COALESCE(reason, '')",
            CostGroupKey::ReasoningEffort => "COALESCE(reasoning_effort, '')",
        };
        self.pool
            .interact("cost.query_range_grouped", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SUMMARY_COLUMNS}, {key_expr} AS bucket \
                     FROM cost_records WHERE timestamp >= ?1 AND timestamp < ?2 \
                     GROUP BY bucket"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params![from, to], bucket_from_aggregate_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
    }

    async fn query_session_by_turn(
        &self,
        session_id: &SessionId,
    ) -> CostResult<Vec<CostGroupBucket>> {
        let session_id = session_id.as_str().to_string();
        self.pool
            .interact("cost.query_session_by_turn", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SUMMARY_COLUMNS}, turn_id AS bucket \
                     FROM cost_records WHERE session_id = ?1 GROUP BY turn_id"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params![session_id], bucket_from_aggregate_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
    }

    async fn query_global(&self, range: TimeRange) -> CostResult<CostSummary> {
        let from = time::to_us(range.from);
        let to = time::to_us(range.to);
        let summary = self
            .pool
            .interact("cost.query_global", move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {SUMMARY_COLUMNS} \
                         FROM cost_records WHERE timestamp >= ?1 AND timestamp < ?2"
                        ),
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
                        &format!(
                            "SELECT {SUMMARY_COLUMNS} \
                         FROM cost_records WHERE session_id = ?1"
                        ),
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
        self
            .pool
            .interact("cost.query_records_in_range", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, session_id, turn_id, span_id, reason, model, reasoning_effort, \
                            input_tokens, output_tokens, cached_input_tokens, \
                            cache_creation_input_tokens, cost_usd, timestamp \
                     FROM cost_records \
                     WHERE timestamp >= ?1 AND timestamp < ?2 \
                     ORDER BY timestamp",
                )?;
                COST.list(stmt.query(rusqlite::params![from, to])?)
            })
            .await
    }

    async fn query_turn(&self, turn_id: &TurnId) -> CostResult<CostSummary> {
        let turn_id = turn_id.to_string();
        let summary = self
            .pool
            .interact("cost.query_turn", move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {SUMMARY_COLUMNS} \
                         FROM cost_records WHERE turn_id = ?1"
                        ),
                        rusqlite::params![turn_id],
                        summary_from_aggregate_row,
                    )
                    .optional()?)
            })
            .await?;
        summary.ok_or_else(|| StorageError::Storage("expected aggregate row".to_string()))
    }
}

const SUMMARY_COLUMNS: &str = "COALESCE(SUM(cost_usd), 0) AS cost_usd, \
    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
    COALESCE(SUM(cached_input_tokens), 0) AS cached_input_tokens, \
    COALESCE(SUM(cache_creation_input_tokens), 0) AS cache_creation_input_tokens, \
    COUNT(*) AS record_count";

fn bucket_from_aggregate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CostGroupBucket> {
    Ok(CostGroupBucket {
        summary: summary_from_aggregate_row(row)?,
        key: row.get::<_, String>("bucket")?,
    })
}

fn summary_from_aggregate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CostSummary> {
    Ok(CostSummary {
        total_cost_usd: MicroUsd::from_micros(row.get::<_, i64>("cost_usd")?),
        total_input_tokens: row.get::<_, i64>("input_tokens")? as usize,
        total_output_tokens: row.get::<_, i64>("output_tokens")? as usize,
        total_cached_input_tokens: row.get::<_, i64>("cached_input_tokens")? as usize,
        total_cache_creation_input_tokens: row.get::<_, i64>("cache_creation_input_tokens")?
            as usize,
        record_count: row.get::<_, i64>("record_count")? as usize,
    })
}

const COST: RowDecoder<CostRecord> = RowDecoder {
    table: "cost_records",
    key: "id",
    decode: |row| {
        let reason: Option<String> = row.get("reason")?;
        Ok(CostRecord {
            user_id: row.get("user_id")?,
            session_id: SessionId::from(row.get::<_, String>("session_id")?),
            turn_id: row.get::<_, String>("turn_id")?.parse::<TurnId>()?,
            span_id: row.get::<_, String>("span_id")?.parse::<SpanId>()?,
            // Legacy NULLs and unknown reason tags both use the default category.
            reason: reason
                .as_deref()
                .and_then(CallReason::parse)
                .unwrap_or_default(),
            model: row.get("model")?,
            reasoning_effort: row.get("reasoning_effort")?,
            input_tokens: row.get::<_, i64>("input_tokens")?.try_into()?,
            output_tokens: row.get::<_, i64>("output_tokens")?.try_into()?,
            cached_input_tokens: row.get::<_, i64>("cached_input_tokens")?.try_into()?,
            cache_creation_input_tokens: row
                .get::<_, i64>("cache_creation_input_tokens")?
                .try_into()?,
            cost_usd: MicroUsd::from_micros(row.get("cost_usd")?),
            timestamp: time::from_us(row.get("timestamp")?)
                .ok_or_else(|| anyhow::anyhow!("invalid timestamp"))?,
        })
    },
};
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn test_record(user_id: &str, cost: MicroUsd) -> CostRecord {
        CostRecord {
            user_id: user_id.to_string(),
            session_id: SessionId::from("sess-1"),
            turn_id: TurnId::new(),
            span_id: SpanId::new(),
            reason: CallReason::Chat,
            model: "gpt-4".to_string(),
            reasoning_effort: None,
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCostStore::new(pool);
        let mut record = test_record("u1", usd(0.05));
        record.reasoning_effort = Some("high".into());
        store.record(&record).await.unwrap();
        let records = store.query_user("u1", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cost_usd, usd(0.05));
        assert_eq!(records[0].reasoning_effort.as_deref(), Some("high"));
    }
    #[tokio::test]
    async fn query_global_summary() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCostStore::new(pool);
        store.record(&test_record("u1", usd(0.10))).await.unwrap();
        store.record(&test_record("u2", usd(0.20))).await.unwrap();
        let summary = store.query_global(wide_range()).await.unwrap();
        assert_eq!(summary.record_count, 2);
        assert_eq!(summary.total_cost_usd, usd(0.30));
    }

    #[tokio::test]
    async fn query_user_summary_aggregates_in_sql() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCostStore::new(pool);
        store.record(&test_record("u1", usd(0.10))).await.unwrap();
        store.record(&test_record("u1", usd(0.20))).await.unwrap();
        store.record(&test_record("u2", usd(0.40))).await.unwrap();
        let summary = store.query_user_summary("u1", wide_range()).await.unwrap();
        assert_eq!(summary.record_count, 2);
        assert_eq!(summary.total_cost_usd, usd(0.30));
        assert_eq!(summary.total_input_tokens, 200);
    }

    #[tokio::test]
    async fn query_range_grouped_by_day_model_reason_and_effort() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCostStore::new(pool);
        let mut a = test_record("u1", usd(0.10));
        a.model = "m-a".into();
        a.reason = CallReason::Chat;
        a.reasoning_effort = Some("high".into());
        let mut b = test_record("u1", usd(0.20));
        b.model = "m-b".into();
        b.reason = CallReason::Tool("WebFetch".into());
        store.record(&a).await.unwrap();
        store.record(&b).await.unwrap();

        let days = store
            .query_range_grouped(wide_range(), CostGroupKey::Day)
            .await
            .unwrap();
        assert_eq!(days.len(), 1, "both records fall on today (UTC)");
        assert_eq!(days[0].key, Utc::now().format("%Y-%m-%d").to_string());
        assert_eq!(days[0].summary.record_count, 2);
        assert_eq!(days[0].summary.total_cost_usd, usd(0.30));

        let mut models = store
            .query_range_grouped(wide_range(), CostGroupKey::Model)
            .await
            .unwrap();
        models.sort_by(|x, y| x.key.cmp(&y.key));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].key, "m-a");
        assert_eq!(models[0].summary.record_count, 1);

        let reasons = store
            .query_range_grouped(wide_range(), CostGroupKey::Reason)
            .await
            .unwrap();
        let keys: Vec<_> = reasons.iter().map(|b| b.key.as_str()).collect();
        assert!(keys.contains(&"chat") && keys.contains(&"tool:WebFetch"));

        let efforts = store
            .query_range_grouped(wide_range(), CostGroupKey::ReasoningEffort)
            .await
            .unwrap();
        let keys: Vec<_> = efforts.iter().map(|b| b.key.as_str()).collect();
        assert!(keys.contains(&"high") && keys.contains(&""));
    }

    #[tokio::test]
    async fn query_session_by_turn_groups_per_turn() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCostStore::new(pool);
        let turn_a = TurnId::new();
        let turn_b = TurnId::new();
        for (turn, cost) in [(turn_a, 0.10), (turn_a, 0.20), (turn_b, 0.40)] {
            let mut rec = test_record("u1", usd(cost));
            rec.turn_id = turn;
            store.record(&rec).await.unwrap();
        }
        let buckets = store
            .query_session_by_turn(&SessionId::from("sess-1"))
            .await
            .unwrap();
        assert_eq!(buckets.len(), 2);
        let a = buckets
            .iter()
            .find(|b| b.key == turn_a.to_string())
            .unwrap();
        assert_eq!(a.summary.record_count, 2);
        assert_eq!(a.summary.total_cost_usd, usd(0.30));
        assert!(
            store
                .query_session_by_turn(&SessionId::from("other"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reason_round_trips() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCostStore::new(pool);
        let mut rec = test_record("u2", usd(0.05));
        rec.reason = CallReason::Tool("WebFetch".to_string());
        store.record(&rec).await.unwrap();
        let records = store.query_user("u2", wide_range()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reason, CallReason::Tool("WebFetch".to_string()));
    }
}
