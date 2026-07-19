//! sqlite implementation of `TraceStore`.
//!
//! Schema lives in `super::mod::init_db`. Each table stores the entity as
//! a single canonical JSON `data` blob; queryable fields surface as
//! VIRTUAL generated columns derived from `json_extract`. SQLite keeps
//! generated columns in lockstep with `data`, so writers only ever set
//! `data` (plus the natural key) — `baybo-trace` owns the rich-type <->
//! row conversion, this layer just shuttles rows.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_model::{JobId, SpanId, StepId};
use baybo_store::trace::Result;
use baybo_store::{SpanEventRow, SpanRow, StepRow, TraceStore};

pub struct SqliteTraceStore {
    pool: SqlitePool,
}

impl SqliteTraceStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceStore for SqliteTraceStore {
    async fn save_step(&self, step: &StepRow) -> Result<()> {
        let id = step.id.to_string();
        let data = step.data.clone();
        self.pool
            .interact("trace.save_step", move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO steps (id, data) VALUES (?1, ?2)",
                    rusqlite::params![id, data],
                )?;
                Ok(())
            })
            .await
    }

    async fn load_step(&self, step_id: &StepId) -> Result<Option<StepRow>> {
        let id = *step_id;
        let key = id.to_string();
        self.pool
            .interact("trace.load_step", move |conn| {
                let data = conn
                    .query_row(
                        "SELECT data FROM steps WHERE id = ?1",
                        rusqlite::params![key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                Ok(data.map(|data| StepRow { id, data }))
            })
            .await
    }

    async fn list_steps_by_job(&self, job_id: &JobId) -> Result<Vec<StepRow>> {
        let job_id = job_id.to_string();
        self.pool
            .interact("trace.list_steps_by_job", move |conn| {
                let mut stmt = conn
                    .prepare("SELECT id, data FROM steps WHERE job_id = ?1 ORDER BY started_at")?;
                let rows = stmt
                    .query_map(rusqlite::params![job_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut out = Vec::with_capacity(rows.len());
                for (id, data) in rows {
                    out.push(StepRow {
                        id: id.parse()?,
                        data,
                    });
                }
                Ok(out)
            })
            .await
    }

    async fn list_unfinished_steps(&self) -> Result<Vec<StepRow>> {
        self.pool
            .interact("trace.list_unfinished_steps", move |conn| {
                // UNION of two partial-index scans (`idx_steps_open`,
                // `idx_spans_open_step`) — an OR-of-EXISTS shape would
                // force a full steps scan with a json_extract per row.
                // UNION also dedups a step that is open on both counts.
                let mut stmt = conn.prepare(
                    "SELECT id, data FROM ( \
                         SELECT id, data, started_at FROM steps WHERE ended_at IS NULL \
                         UNION \
                         SELECT st.id, st.data, st.started_at FROM steps st \
                             JOIN spans sp ON sp.step_id = st.id \
                             WHERE sp.ended_at IS NULL \
                     ) ORDER BY started_at",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut out = Vec::with_capacity(rows.len());
                for (id, data) in rows {
                    out.push(StepRow {
                        id: id.parse()?,
                        data,
                    });
                }
                Ok(out)
            })
            .await
    }

    async fn save_span(&self, span: &SpanRow) -> Result<()> {
        let id = span.id.to_string();
        let data = span.data.clone();
        self.pool
            .interact("trace.save_span", move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO spans (id, data) VALUES (?1, ?2)",
                    rusqlite::params![id, data],
                )?;
                Ok(())
            })
            .await
    }

    async fn load_span(&self, span_id: &SpanId) -> Result<Option<SpanRow>> {
        let id = *span_id;
        let key = id.to_string();
        self.pool
            .interact("trace.load_span", move |conn| {
                let data = conn
                    .query_row(
                        "SELECT data FROM spans WHERE id = ?1",
                        rusqlite::params![key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                Ok(data.map(|data| SpanRow { id, data }))
            })
            .await
    }

    async fn list_spans_by_step(&self, step_id: &StepId) -> Result<Vec<SpanRow>> {
        let step_id = step_id.to_string();
        self.pool
            .interact("trace.list_spans_by_step", move |conn| {
                let mut stmt = conn
                    .prepare("SELECT id, data FROM spans WHERE step_id = ?1 ORDER BY started_at")?;
                let rows = stmt
                    .query_map(rusqlite::params![step_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut out = Vec::with_capacity(rows.len());
                for (id, data) in rows {
                    out.push(SpanRow {
                        id: id.parse()?,
                        data,
                    });
                }
                Ok(out)
            })
            .await
    }

    async fn list_spans_by_job(&self, job_id: &JobId) -> Result<Vec<SpanRow>> {
        let job_id = job_id.to_string();
        self.pool
            .interact("trace.list_spans_by_job", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT sp.id, sp.data FROM spans sp \
                     JOIN steps st ON sp.step_id = st.id \
                     WHERE st.job_id = ?1 ORDER BY sp.started_at",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![job_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut out = Vec::with_capacity(rows.len());
                for (id, data) in rows {
                    out.push(SpanRow {
                        id: id.parse()?,
                        data,
                    });
                }
                Ok(out)
            })
            .await
    }

    async fn trace_counts_by_job(&self, job_id: &JobId) -> Result<(usize, usize)> {
        let job_id = job_id.to_string();
        self.pool
            .interact("trace.trace_counts_by_job", move |conn| {
                let (steps, spans): (i64, i64) = conn.query_row(
                    "SELECT (SELECT COUNT(*) FROM steps WHERE job_id = ?1), \
                            (SELECT COUNT(*) FROM spans WHERE step_id IN \
                                 (SELECT id FROM steps WHERE job_id = ?1))",
                    rusqlite::params![job_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok((steps as usize, spans as usize))
            })
            .await
    }

    async fn append_span_event(&self, event: &SpanEventRow) -> Result<()> {
        let span_id = event.span_id.to_string();
        let seq = event.seq as i64;
        let data = event.data.clone();
        self.pool
            .interact("trace.append_span_event", move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO span_events (span_id, seq, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![span_id, seq, data],
                )?;
                Ok(())
            })
            .await
    }

    async fn list_span_events(&self, span_id: &SpanId) -> Result<Vec<SpanEventRow>> {
        let span_id = *span_id;
        let key = span_id.to_string();
        self.pool
            .interact("trace.list_span_events", move |conn| {
                let mut stmt = conn
                    .prepare("SELECT seq, data FROM span_events WHERE span_id = ?1 ORDER BY seq")?;
                let out = stmt
                    .query_map(rusqlite::params![key], |row| {
                        Ok(SpanEventRow {
                            span_id,
                            seq: row.get::<_, i64>(0)? as u32,
                            data: row.get::<_, String>(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(out)
            })
            .await
    }

    async fn list_span_events_for_spans(&self, span_ids: &[SpanId]) -> Result<Vec<SpanEventRow>> {
        if span_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Chunk the IN list to stay well under sqlite's bound-variable
        // limit. A span id lives in exactly one chunk, so per-span
        // `(span_id, seq)` ordering survives concatenation.
        const IN_CHUNK: usize = 500;
        let mut out = Vec::new();
        for chunk in span_ids.chunks(IN_CHUNK) {
            let keys: Vec<String> = chunk.iter().map(|s| s.to_string()).collect();
            let rows = self
                .pool
                .interact("trace.list_span_events_for_spans", move |conn| {
                    let placeholders = super::in_placeholders(keys.len());
                    let mut stmt = conn.prepare(&format!(
                        "SELECT span_id, seq, data FROM span_events \
                         WHERE span_id IN ({placeholders}) ORDER BY span_id, seq"
                    ))?;
                    let raw = stmt
                        .query_map(rusqlite::params_from_iter(keys.iter()), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    let mut rows = Vec::with_capacity(raw.len());
                    for (span_id, seq, data) in raw {
                        rows.push(SpanEventRow {
                            span_id: span_id.parse()?,
                            seq: seq as u32,
                            data,
                        });
                    }
                    Ok(rows)
                })
                .await?;
            out.extend(rows);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_trace::{
        LifecycleOutcome, LifecycleState, LlmCallInputs, Span, SpanEvent, SpanEventKind, SpanKind,
        Step, StepKind, ToolCallOrigin,
    };
    use chrono::Utc;

    fn make_step(job_id: JobId) -> Step {
        Step {
            id: StepId::new(),
            job_id,
            kind: StepKind::LlmIteration,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleState::Pending,
        }
    }

    fn make_llm_span(step_id: StepId) -> Span {
        Span {
            id: SpanId::new(),
            step_id,
            kind: SpanKind::LlmCall {
                begin: baybo_trace::LlmCallBegin {
                    model_id: "claude".into(),
                    provider: "anthropic".into(),
                    provider_config_hash: "h".into(),
                    input_messages: LlmCallInputs::empty(),
                    temperature: None,
                },
                result: None,
            },
            parallel_group: None,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleState::Pending,
            events: vec![],
        }
    }

    fn make_tool_span(step_id: StepId, llm_span_id: SpanId) -> Span {
        Span {
            id: SpanId::new(),
            step_id,
            kind: SpanKind::ToolCall {
                begin: baybo_trace::ToolCallBegin {
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
            outcome: LifecycleState::Pending,
            events: vec![],
        }
    }

    #[tokio::test]
    async fn step_round_trip() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);
        let s = make_step(JobId::new());
        store.save_step(&s.to_row().unwrap()).await.unwrap();
        let loaded = Step::from_row(store.load_step(&s.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(loaded.id, s.id);
    }

    #[tokio::test]
    async fn span_round_trip_and_list_by_step() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);
        let job_id = JobId::new();
        let step = make_step(job_id);
        store.save_step(&step.to_row().unwrap()).await.unwrap();

        let llm = make_llm_span(step.id);
        store.save_span(&llm.to_row().unwrap()).await.unwrap();
        let tool = make_tool_span(step.id, llm.id);
        store.save_span(&tool.to_row().unwrap()).await.unwrap();

        let spans = store.list_spans_by_step(&step.id).await.unwrap();
        assert_eq!(spans.len(), 2);
    }

    #[tokio::test]
    async fn list_steps_by_job_filters_via_generated_column() {
        // Exercises the `steps.job_id` VIRTUAL column
        // (`json_extract(data, '$.job_id')`) — the lookup path the
        // round-trip/load tests don't touch. Guards against a schema
        // path typo or a rename of `Step::job_id` silently dropping
        // every step out of the by-job query.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);
        let job_a = JobId::new();
        let job_b = JobId::new();

        let a1 = make_step(job_a);
        let a2 = make_step(job_a);
        let b1 = make_step(job_b);
        for s in [&a1, &a2, &b1] {
            store.save_step(&s.to_row().unwrap()).await.unwrap();
        }

        let got = store.list_steps_by_job(&job_a).await.unwrap();
        let got_ids: Vec<_> = got.iter().map(|r| r.id).collect();
        assert_eq!(got.len(), 2, "only the two job_a steps should match");
        assert!(got_ids.contains(&a1.id) && got_ids.contains(&a2.id));

        assert_eq!(store.list_steps_by_job(&job_b).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_spans_by_job_joins_through_steps() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);
        let job_a = JobId::new();
        let job_b = JobId::new();
        let a1 = make_step(job_a);
        let a2 = make_step(job_a);
        let b1 = make_step(job_b);
        for s in [&a1, &a2, &b1] {
            store.save_step(&s.to_row().unwrap()).await.unwrap();
        }
        let s1 = make_llm_span(a1.id);
        let s2 = make_llm_span(a2.id);
        let s3 = make_llm_span(b1.id);
        for sp in [&s1, &s2, &s3] {
            store.save_span(&sp.to_row().unwrap()).await.unwrap();
        }

        let got = store.list_spans_by_job(&job_a).await.unwrap();
        let got_ids: Vec<_> = got.iter().map(|r| r.id).collect();
        assert_eq!(got.len(), 2, "only job_a's spans across both its steps");
        assert!(got_ids.contains(&s1.id) && got_ids.contains(&s2.id));
        assert!(
            store
                .list_spans_by_job(&JobId::new())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn list_span_events_for_spans_batches_and_keeps_seq_order() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);
        let span_a = SpanId::new();
        let span_b = SpanId::new();
        let span_without_events = SpanId::new();
        for (span, seq) in [(span_a, 0), (span_a, 1), (span_b, 0)] {
            let event = SpanEvent::new(
                span,
                seq,
                SpanEventKind::Approval {
                    decision: baybo_model::ApprovalDecision::Approve,
                    resource: baybo_model::ResourceAccess::ReadFile {
                        path: std::path::PathBuf::from("/tmp/foo"),
                    },
                },
            );
            store
                .append_span_event(&event.to_row().unwrap())
                .await
                .unwrap();
        }

        let rows = store
            .list_span_events_for_spans(&[span_a, span_b, span_without_events])
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        let a_seqs: Vec<u32> = rows
            .iter()
            .filter(|r| r.span_id == span_a)
            .map(|r| r.seq)
            .collect();
        assert_eq!(a_seqs, vec![0, 1], "per-span seq order preserved");
        assert!(
            store
                .list_span_events_for_spans(&[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn trace_counts_by_job_counts_across_steps_without_loading() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);
        let job_a = JobId::new();
        let job_b = JobId::new();

        // job_a: two steps carrying 2 + 1 spans; job_b: one step, one span.
        let a1 = make_step(job_a);
        let a2 = make_step(job_a);
        let b1 = make_step(job_b);
        for s in [&a1, &a2, &b1] {
            store.save_step(&s.to_row().unwrap()).await.unwrap();
        }
        for span in [
            make_llm_span(a1.id),
            make_llm_span(a1.id),
            make_llm_span(a2.id),
            make_llm_span(b1.id),
        ] {
            store.save_span(&span.to_row().unwrap()).await.unwrap();
        }

        assert_eq!(store.trace_counts_by_job(&job_a).await.unwrap(), (2, 3));
        assert_eq!(store.trace_counts_by_job(&job_b).await.unwrap(), (1, 1));
        assert_eq!(
            store.trace_counts_by_job(&JobId::new()).await.unwrap(),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn list_unfinished_steps_finds_open_steps_and_open_child_spans() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);

        let open_step = make_step(JobId::new());
        store.save_step(&open_step.to_row().unwrap()).await.unwrap();

        let step_with_open_span = Step {
            id: StepId::new(),
            job_id: JobId::new(),
            kind: StepKind::Compression,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            outcome: LifecycleState::Done(LifecycleOutcome::Ok),
        };
        store
            .save_step(&step_with_open_span.to_row().unwrap())
            .await
            .unwrap();
        let open_span = make_llm_span(step_with_open_span.id);
        store.save_span(&open_span.to_row().unwrap()).await.unwrap();

        let finished_step = Step {
            id: StepId::new(),
            job_id: JobId::new(),
            kind: StepKind::LlmIteration,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            outcome: LifecycleState::Done(LifecycleOutcome::Ok),
        };
        store
            .save_step(&finished_step.to_row().unwrap())
            .await
            .unwrap();
        let mut finished_span = make_llm_span(finished_step.id);
        finished_span.ended_at = Some(Utc::now());
        finished_span.outcome = LifecycleState::Done(LifecycleOutcome::Ok);
        store
            .save_span(&finished_span.to_row().unwrap())
            .await
            .unwrap();

        let ids: std::collections::HashSet<_> = store
            .list_unfinished_steps()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(ids.contains(&open_step.id));
        assert!(ids.contains(&step_with_open_span.id));
        assert!(!ids.contains(&finished_step.id));
    }

    #[tokio::test]
    async fn span_event_round_trip() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteTraceStore::new(pool);
        let span_id = SpanId::new();
        let event = SpanEvent::new(
            span_id,
            0,
            SpanEventKind::Approval {
                decision: baybo_model::ApprovalDecision::Approve,
                resource: baybo_model::ResourceAccess::ReadFile {
                    path: std::path::PathBuf::from("/tmp/foo"),
                },
            },
        );
        store
            .append_span_event(&event.to_row().unwrap())
            .await
            .unwrap();
        let listed = store.list_span_events(&span_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].seq, 0);
    }
}
