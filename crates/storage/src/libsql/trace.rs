//! libsql implementation of `TraceStore`.
//!
//! Schema lives in `super::mod::init_db`. Each table stores the entity
//! as a single canonical JSON `data` blob; queryable fields surface as
//! VIRTUAL generated columns derived from `json_extract`. SQLite keeps
//! generated columns in lockstep with `data`, so writers only ever set
//! `data` — no two-side write contract for the storage layer to police.

use async_trait::async_trait;

use super::LibsqlPool;
use crate::trace::TraceStore;
use aura_model::{JobId, SpanId, StepId};
use aura_trace::{Span, SpanEvent, Step, TraceError};

pub struct LibsqlTraceStore {
    pool: LibsqlPool,
}

impl LibsqlTraceStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceStore for LibsqlTraceStore {
    async fn save_step(&self, step: &Step) -> aura_trace::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(step)
            .map_err(|e| TraceError::Storage(format!("serialize step: {e}")))?;
        conn.execute(
            "INSERT OR REPLACE INTO steps (id, data) VALUES (?1, ?2)",
            libsql::params![step.id.to_string(), data],
        )
        .await
        .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql insert step: {e}")))?;
        Ok(())
    }

    async fn load_step(&self, step_id: &StepId) -> aura_trace::Result<Option<Step>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM steps WHERE id = ?1 AND deleted_at IS NULL",
                libsql::params![step_id.to_string()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        match rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                Ok(Some(serde_json::from_str(&data).map_err(|e| {
                    TraceError::Storage(format!("deserialize step: {e}"))
                })?))
            }
            None => Ok(None),
        }
    }

    async fn list_steps_by_job(&self, job_id: &JobId) -> aura_trace::Result<Vec<Step>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM steps \
                 WHERE job_id = ?1 AND deleted_at IS NULL ORDER BY started_at",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        let mut steps = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            steps.push(
                serde_json::from_str(&data)
                    .map_err(|e| TraceError::Storage(format!("deserialize step: {e}")))?,
            );
        }
        Ok(steps)
    }

    async fn save_span(&self, span: &Span) -> aura_trace::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(span)
            .map_err(|e| TraceError::Storage(format!("serialize span: {e}")))?;
        conn.execute(
            "INSERT OR REPLACE INTO spans (id, data) VALUES (?1, ?2)",
            libsql::params![span.id.to_string(), data],
        )
        .await
        .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql insert span: {e}")))?;
        Ok(())
    }

    async fn load_span(&self, span_id: &SpanId) -> aura_trace::Result<Option<Span>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM spans WHERE id = ?1 AND deleted_at IS NULL",
                libsql::params![span_id.to_string()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        match rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                Ok(Some(serde_json::from_str(&data).map_err(|e| {
                    TraceError::Storage(format!("deserialize span: {e}"))
                })?))
            }
            None => Ok(None),
        }
    }

    async fn list_spans_by_step(&self, step_id: &StepId) -> aura_trace::Result<Vec<Span>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM spans \
                 WHERE step_id = ?1 AND deleted_at IS NULL ORDER BY started_at",
                libsql::params![step_id.to_string()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        let mut spans = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            spans.push(
                serde_json::from_str(&data)
                    .map_err(|e| TraceError::Storage(format!("deserialize span: {e}")))?,
            );
        }
        Ok(spans)
    }

    async fn append_span_event(&self, event: &SpanEvent) -> aura_trace::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(event)
            .map_err(|e| TraceError::Storage(format!("serialize span_event: {e}")))?;
        conn.execute(
            "INSERT OR REPLACE INTO span_events (span_id, seq, data) VALUES (?1, ?2, ?3)",
            libsql::params![event.span_id.to_string(), event.seq as i64, data],
        )
        .await
        .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql insert span_event: {e}")))?;
        Ok(())
    }

    async fn list_span_events(&self, span_id: &SpanId) -> aura_trace::Result<Vec<SpanEvent>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM span_events \
                 WHERE span_id = ?1 AND deleted_at IS NULL ORDER BY seq",
                libsql::params![span_id.to_string()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        let mut events = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            events.push(
                serde_json::from_str(&data)
                    .map_err(|e| TraceError::Storage(format!("deserialize span_event: {e}")))?,
            );
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_trace::{LifecycleOutcome, SpanEventKind, SpanKind, StepKind, ToolCallOrigin};
    use chrono::Utc;

    fn make_step(job_id: JobId) -> Step {
        Step {
            id: StepId::new(),
            job_id,
            kind: StepKind::LlmIteration,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
        }
    }

    fn make_llm_span(step_id: StepId) -> Span {
        Span {
            id: SpanId::new(),
            step_id,
            kind: SpanKind::LlmCall {
                begin: aura_trace::LlmCallBegin {
                    model_id: "claude".into(),
                    provider: "anthropic".into(),
                    provider_config_hash: "h".into(),
                    input_messages: vec![],
                    temperature: None,
                },
                result: None,
            },
            parallel_group: None,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
            events: vec![],
        }
    }

    fn make_tool_span(step_id: StepId, llm_span_id: SpanId) -> Span {
        Span {
            id: SpanId::new(),
            step_id,
            kind: SpanKind::ToolCall {
                begin: aura_trace::ToolCallBegin {
                    tool_name: "bash".into(),
                    tool_artifact_hash: "h".into(),
                    triggered_by: Some(ToolCallOrigin {
                        llm_span_id,
                        tool_use_id: "tu1".into(),
                    }),
                    params: serde_json::json!({}),
                },
                result: None,
            },
            parallel_group: None,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
            events: vec![],
        }
    }

    #[tokio::test]
    async fn step_round_trip() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);
        let s = make_step(JobId::new());
        store.save_step(&s).await.unwrap();
        let loaded = store.load_step(&s.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, s.id);
    }

    #[tokio::test]
    async fn span_round_trip_and_list_by_step() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);
        let job_id = JobId::new();
        let step = make_step(job_id);
        store.save_step(&step).await.unwrap();

        let llm = make_llm_span(step.id);
        store.save_span(&llm).await.unwrap();
        let tool = make_tool_span(step.id, llm.id);
        store.save_span(&tool).await.unwrap();

        let spans = store.list_spans_by_step(&step.id).await.unwrap();
        assert_eq!(spans.len(), 2);
    }

    #[tokio::test]
    async fn span_event_round_trip() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);
        let span_id = SpanId::new();
        let event = SpanEvent::new(
            span_id,
            0,
            SpanEventKind::Approval {
                decision: aura_model::ApprovalDecision::Approve,
                resource: aura_model::ResourceAccess::ReadFile {
                    path: std::path::PathBuf::from("/tmp/foo"),
                },
            },
        );
        store.append_span_event(&event).await.unwrap();
        let listed = store.list_span_events(&span_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].seq, 0);
    }
}
