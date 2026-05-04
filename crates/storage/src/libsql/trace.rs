//! libsql implementation of `TraceStore` + `TraceEventStore`.
//!
//! Schema lives in `super::mod::init_db` (`steps`, `spans`,
//! `span_events`, `trace_events`). Soft-delete + recovery semantics are
//! enforced here.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use crate::trace::{TraceEventStore, TraceLogEvent, TraceStore};
use aura_model::{JobId, SessionId, SpanId, StepId};
use aura_trace::{
    LifecycleOutcome, RecoveredSpan, RecoveryReport, Span, SpanEvent, SpanEventKind, SpanKind,
    Step, StepKind, TraceError,
};

pub struct LibsqlTraceStore {
    pool: LibsqlPool,
}

impl LibsqlTraceStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

pub struct LibsqlTraceEventStore {
    pool: LibsqlPool,
}

impl LibsqlTraceEventStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

fn step_kind_str(k: &StepKind) -> &'static str {
    match k {
        StepKind::LlmIteration => "llm_iteration",
        StepKind::ToolDirect => "tool_direct",
        StepKind::Compression => "compression",
        StepKind::MemoryRecall => "memory_recall",
        StepKind::MemoryWrite => "memory_write",
        StepKind::SkillSelection => "skill_selection",
        StepKind::Subagent { .. } => "subagent",
    }
}

fn span_kind_str(k: &SpanKind) -> &'static str {
    match k {
        SpanKind::LlmCall { .. } => "llm_call",
        SpanKind::ToolCall { .. } => "tool_call",
        SpanKind::SubagentStub { .. } => "subagent_stub",
    }
}

fn span_event_kind_str(k: &SpanEventKind) -> &'static str {
    match k {
        SpanEventKind::SanitizeHit { .. } => "sanitize_hit",
        SpanEventKind::Approval { .. } => "approval",
        SpanEventKind::HookDegraded { .. } => "hook_degraded",
    }
}

fn outcome_str(o: &LifecycleOutcome) -> &'static str {
    match o {
        LifecycleOutcome::Pending => "pending",
        LifecycleOutcome::Ok => "ok",
        LifecycleOutcome::Failed { .. } => "failed",
        LifecycleOutcome::Cancelled { .. } => "cancelled",
    }
}

#[async_trait]
impl TraceStore for LibsqlTraceStore {
    async fn save_step(&self, step: &Step) -> aura_trace::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(step)
            .map_err(|e| TraceError::Storage(format!("serialize step: {e}")))?;
        conn.execute(
            "INSERT OR REPLACE INTO steps \
             (id, job_id, kind, started_at, ended_at, outcome, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            libsql::params![
                step.id.to_string(),
                step.job_id.to_string(),
                step_kind_str(&step.kind).to_string(),
                super::time::to_us(step.started_at),
                step.ended_at.map(super::time::to_us),
                outcome_str(&step.outcome).to_string(),
                data,
            ],
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
        let row = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?;
        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let s: Step = serde_json::from_str(&data)
                    .map_err(|e| TraceError::Storage(format!("deserialize step: {e}")))?;
                Ok(Some(s))
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
            "INSERT OR REPLACE INTO spans \
             (id, step_id, kind, parallel_group, started_at, ended_at, outcome, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            libsql::params![
                span.id.to_string(),
                span.step_id.to_string(),
                span_kind_str(&span.kind).to_string(),
                span.parallel_group.map(|g| g.to_string()),
                super::time::to_us(span.started_at),
                span.ended_at.map(super::time::to_us),
                outcome_str(&span.outcome).to_string(),
                data,
            ],
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
        let row = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?;
        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let s: Span = serde_json::from_str(&data)
                    .map_err(|e| TraceError::Storage(format!("deserialize span: {e}")))?;
                Ok(Some(s))
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
            "INSERT OR REPLACE INTO span_events (span_id, seq, at, kind, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            libsql::params![
                event.span_id.to_string(),
                event.seq as i64,
                super::time::to_us(event.at),
                span_event_kind_str(&event.kind).to_string(),
                data,
            ],
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

    async fn recover_half_open_spans(&self) -> aura_trace::Result<RecoveryReport> {
        let conn = self.pool.conn();
        // Find half-open spans + their parent job (joining via the step).
        let mut rows = conn
            .query(
                "SELECT spans.id, steps.job_id, spans.data \
                 FROM spans \
                 JOIN steps ON steps.id = spans.step_id \
                 WHERE spans.ended_at IS NULL \
                   AND spans.deleted_at IS NULL",
                (),
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let now = Utc::now();
        let mut recovered = Vec::new();
        let mut to_rewrite: Vec<(String, Span)> = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let span_id_str: String = row
                .get(0)
                .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let job_id_str: String = row
                .get(1)
                .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let data: String = row
                .get(2)
                .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let span: Span = serde_json::from_str(&data)
                .map_err(|e| TraceError::Storage(format!("deserialize span: {e}")))?;
            let job_id: JobId = job_id_str
                .parse()
                .map_err(|e| TraceError::Storage(format!("decode job_id: {e}")))?;
            recovered.push(RecoveredSpan::for_system_crash(job_id, span.id, now));
            to_rewrite.push((span_id_str, span));
        }

        // Rewrite each half-open span: stamp ended_at + cancelled outcome.
        for (span_id, mut span) in to_rewrite {
            span.ended_at = Some(now);
            span.outcome = LifecycleOutcome::Cancelled {
                reason: aura_job::CancelReason::SystemCrash,
            };
            let data = serde_json::to_string(&span)
                .map_err(|e| TraceError::Storage(format!("reserialize span: {e}")))?;
            conn.execute(
                "UPDATE spans SET ended_at = ?1, outcome = 'cancelled', data = ?2 \
                 WHERE id = ?3 AND ended_at IS NULL AND deleted_at IS NULL",
                libsql::params![super::time::to_us(now), data, span_id],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql update: {e}")))?;
        }
        Ok(RecoveryReport::new(recovered))
    }
}

#[async_trait]
impl TraceEventStore for LibsqlTraceEventStore {
    async fn append(&self, event: &TraceLogEvent) -> aura_trace::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(event)
            .map_err(|e| TraceError::Storage(format!("serialize trace_event: {e}")))?;
        conn.execute(
            "INSERT INTO trace_events (session_id, job_id, at, data) \
             VALUES (?1, ?2, ?3, ?4)",
            libsql::params![
                event.session_id.as_str().to_string(),
                event.job_id.to_string(),
                super::time::to_us(event.at),
                data,
            ],
        )
        .await
        .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql insert trace_event: {e}")))?;
        Ok(())
    }

    async fn replay_session(
        &self,
        session_id: &SessionId,
    ) -> aura_trace::Result<Vec<TraceLogEvent>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM trace_events \
                 WHERE session_id = ?1 AND deleted_at IS NULL ORDER BY id",
                libsql::params![session_id.as_str().to_string()],
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
                    .map_err(|e| TraceError::Storage(format!("deserialize trace_event: {e}")))?,
            );
        }
        Ok(events)
    }

    async fn compact_before(&self, cutoff: DateTime<Utc>) -> aura_trace::Result<u64> {
        let conn = self.pool.conn();
        let now = super::time::now_us();
        let affected = conn
            .execute(
                "UPDATE trace_events SET deleted_at = ?1 \
                 WHERE at < ?2 AND deleted_at IS NULL",
                libsql::params![now, super::time::to_us(cutoff)],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql update: {e}")))?;
        Ok(affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_trace::ToolCallOrigin;

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
                model_id: "claude".into(),
                provider: "anthropic".into(),
                provider_config_hash: "h".into(),
                input_messages: vec![],
                temperature: None,
                output_content: String::new(),
                thinking: None,
                tool_calls: vec![],
                input_tokens: 0,
                output_tokens: 0,
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
                tool_name: "bash".into(),
                tool_artifact_hash: "h".into(),
                triggered_by: Some(ToolCallOrigin {
                    llm_span_id,
                    tool_use_id: "tu1".into(),
                }),
                params: serde_json::json!({}),
                output: serde_json::json!({}),
                success: false,
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
    async fn recover_half_open_spans_marks_cancelled() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);
        let job_id = JobId::new();
        let step = make_step(job_id);
        store.save_step(&step).await.unwrap();
        let span = make_llm_span(step.id);
        store.save_span(&span).await.unwrap();

        let report = store.recover_half_open_spans().await.unwrap();
        assert_eq!(report.recovered.len(), 1);
        assert_eq!(report.recovered[0].job_id, job_id);
        assert_eq!(report.recovered[0].span_id, span.id);
        assert!(matches!(
            report.recovered[0].new_outcome,
            LifecycleOutcome::Cancelled {
                reason: aura_job::CancelReason::SystemCrash,
            }
        ));

        // Idempotent: re-running yields no new recoveries
        let again = store.recover_half_open_spans().await.unwrap();
        assert!(again.recovered.is_empty());
    }

    #[tokio::test]
    async fn span_event_round_trip() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);
        let span_id = SpanId::new();
        let event = SpanEvent::new(
            span_id,
            0,
            SpanEventKind::HookDegraded {
                hook_name: "h".into(),
                timeout_ms: 100,
                phase: aura_model::HookPhase::PreStep,
            },
        );
        store.append_span_event(&event).await.unwrap();
        let listed = store.list_span_events(&span_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].seq, 0);
    }

    #[tokio::test]
    async fn trace_event_log_round_trips() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceEventStore::new(pool);
        let session_id = SessionId::from("cli-1");
        let job_id = JobId::new();
        let step_id = StepId::new();
        let event = TraceLogEvent {
            session_id: session_id.clone(),
            job_id,
            at: Utc::now(),
            kind: crate::trace::TraceLogEventKind::StepBegin {
                step_id,
                payload: serde_json::json!({}),
            },
        };
        store.append(&event).await.unwrap();
        let replayed = store.replay_session(&session_id).await.unwrap();
        assert_eq!(replayed.len(), 1);
    }
}
