//! Query API v1 — the read surface over Session / Job / Step / Span,
//! defined as the union of the read paths in `docs/modules/{session,job,trace}.md`.
//!
//! 9 endpoints:
//!
//! 1. `load_session` — resolves lineage
//! 2. `list_jobs` — job summaries for a session
//! 3. `load_job` — Job + step list
//! 4. `load_step` — Step + spans + span events
//! 5. `find_recoverable_jobs` — recovery scan
//! 6. `list_active_subagents` — live Subagent-lineage children
//! 7. `lineage_tree` — ancestry + immediate descendants
//! 8. `cost_summary` — User / Session / Job / TimeRange
//! 9. `replay` — chronological Job → Step → Span tree
//!
//! Errors collapse into a single `QueryError` so callers don't need to
//! match four different store error types.

use std::collections::HashMap;
use std::sync::Arc;

use baybo_cost::{CostError, CostStore, CostSummary, TimeRange};
use baybo_job::{Job, JobError, JobInputKind, JobLifecycle, JobStatus, JobStatusKind};
use baybo_model::{
    CallReason, JobId, Lineage, LineageKind, MicroUsd, Session, SessionId, StepId, TriggerKind,
};
use baybo_session::{SessionError, SessionStore, StoredMessage};
use baybo_trace::{Span, SpanEvent, Step, TraceError, TraceStore};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    #[error("job error: {0}")]
    Job(#[from] JobError),
    #[error("trace error: {0}")]
    Trace(#[from] TraceError),
    #[error("cost error: {0}")]
    Cost(#[from] CostError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, QueryError>;

// ── DTOs ────────────────────────────────────────────────────────────

/// Filter for `list_jobs`. All fields are AND-combined; `None` means
/// no constraint.
#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    pub status_kind: Option<JobStatusKind>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// Lightweight row returned by `list_jobs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: JobId,
    pub session_id: SessionId,
    pub input_kind: JobInputKind,
    pub origin: TriggerKind,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl JobSummary {
    fn from_owned(j: &Job) -> Self {
        Self {
            id: j.id,
            session_id: j.session_id.clone(),
            input_kind: j.input_kind(),
            origin: j.origin,
            status: j.status.clone(),
            created_at: j.created_at,
            started_at: j.started_at,
            ended_at: j.ended_at,
        }
    }
}

/// Job + its step list. Step children are *not* eagerly loaded; call
/// `load_step` for each step's spans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDetail {
    pub job: Job,
    pub steps: Vec<Step>,
}

/// Step + every span under it (plus each span's `events` already
/// inlined by the `Span` struct). Heavier than `JobDetail::steps`
/// because it eagerly fetches spans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDetail {
    pub step: Step,
    pub spans: Vec<Span>,
}

/// One node in the lineage tree returned by `lineage_tree`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageNode {
    pub session_id: SessionId,
    /// `Some` when this node is the descendant of another via
    /// `Lineage`. `None` for the root node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_kind: Option<LineageKind>,
    pub children: Vec<LineageNode>,
}

fn build_lineage_node(
    id: SessionId,
    via_kind: Option<LineageKind>,
    edges: &HashMap<SessionId, Vec<(SessionId, LineageKind)>>,
) -> LineageNode {
    let children = edges
        .get(&id)
        .map(|kids| {
            kids.iter()
                .map(|(cid, kind)| build_lineage_node(cid.clone(), Some(kind.clone()), edges))
                .collect()
        })
        .unwrap_or_default();
    LineageNode {
        session_id: id,
        via_kind,
        children,
    }
}

/// Scope for `cost_summary`.
#[derive(Debug, Clone)]
pub enum CostScope {
    User { user_id: String, range: TimeRange },
    Session(SessionId),
    Job(JobId),
    TimeRange(TimeRange),
}

/// Chronological replay of a session's job/step/span tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayedConversation {
    pub session_id: SessionId,
    pub jobs: Vec<ReplayJob>,
}

/// Coarse "what started this session" label derived from
/// `session.trigger` and `session.lineage` for the trace browser. Pure
/// presentation — never persisted, never round-tripped.
///
/// Variants are exhaustive over the (trigger, lineage) combinations we
/// surface today. Subagent overrides trigger because a subagent of a
/// cron job is still conceptually "a subagent" for browsing purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Root user-triggered chat session.
    User,
    /// Root cron-triggered session.
    Cron,
    /// Subagent spawned by another session — its trigger is inherited
    /// from the root, but `lineage.kind == Subagent` wins for display.
    Subagent,
}

fn derive_session_kind(session: &Session) -> SessionKind {
    if let Some(Lineage {
        kind: LineageKind::Subagent,
        ..
    }) = session.lineage.as_ref()
    {
        return SessionKind::Subagent;
    }
    match session.trigger {
        baybo_model::TriggerSource::Cron { .. } => SessionKind::Cron,
        baybo_model::TriggerSource::User => SessionKind::User,
    }
}

/// Filter for [`QueryApi::list_session_summaries`]. All fields are
/// AND-combined; `None` means no constraint. `status_kind` matches
/// against the *latest* job's `JobStatusKind` (not any historical job).
#[derive(Debug, Clone, Default)]
pub struct SessionSummaryFilter {
    pub status_kind: Option<JobStatusKind>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub session_id_prefix: Option<String>,
    /// Coarse trigger/lineage label. Filters the `list_all` pool by
    /// [`derive_session_kind`]. `Compression` matches no live row
    /// (background compression runs as an in-actor step), so that
    /// variant yields an empty listing.
    pub kind: Option<SessionKind>,
}

/// Offset/limit pagination for [`QueryApi::list_session_summaries`].
/// `limit == 0` is treated as "no limit" — the full filtered list is
/// returned. `offset` past the end yields an empty page.
#[derive(Debug, Clone, Copy)]
pub struct SessionSummaryPage {
    pub offset: usize,
    pub limit: usize,
}

/// One row of the trace browser list view. Carries the cheap
/// per-session aggregates the UI needs to render the table — full
/// drill-in still goes through [`QueryApi::replay`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    /// `None` when the session has no jobs (filtered out by default).
    pub latest_job_status: Option<JobStatus>,
    pub kind: SessionKind,
    pub job_count: usize,
    pub span_count: usize,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
}

/// Result of [`QueryApi::list_session_summaries`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummaryListing {
    pub items: Vec<SessionSummary>,
    /// Total rows matching the filter, before pagination — drives the
    /// `Showing X to Y of N` pager.
    pub total: usize,
}

/// Result of [`QueryApi::compute_analytics`]. All counts/totals are
/// over the supplied [`TimeRange`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyticsSummary {
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub total_cached_input_tokens: usize,
    pub total_cache_creation_input_tokens: usize,
    pub total_cost_usd: MicroUsd,
    pub total_record_count: usize,
    /// One bucket per UTC day in the range, oldest first. Days with no
    /// activity still appear with zeros so the chart can render a
    /// continuous x-axis.
    pub daily: Vec<AnalyticsDayBucket>,
    pub by_model: Vec<AnalyticsModelBucket>,
    /// One bucket per [`CallReason`], so spend can be read by purpose
    /// (chat vs compression vs tool vs …). Sorted by token volume, desc.
    pub by_reason: Vec<AnalyticsReasonBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsDayBucket {
    /// `YYYY-MM-DD` (UTC).
    pub date: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    pub cost_usd: MicroUsd,
    /// Distinct sessions whose `created_at` falls in this UTC day.
    pub sessions_created: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsModelBucket {
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    pub cost_usd: MicroUsd,
    pub call_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsReasonBucket {
    pub reason: CallReason,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    pub cost_usd: MicroUsd,
    pub call_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayJob {
    pub job: Job,
    pub steps: Vec<ReplayStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayStep {
    pub step: Step,
    pub spans: Vec<Span>,
}

/// Wire-friendly mirror of [`baybo_session::StoredMessage`]. Carried
/// once at the top of [`TraceOverview`] so the client can hydrate
/// every `LlmCallInputs::Persisted { last_ordinal }` span locally
/// instead of the server re-inlining the same prefix per span — the
/// duplicated payload that motivates the split in the first place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessageRow {
    pub ordinal: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub message: baybo_model::ChatMessage,
}

impl From<StoredMessage> for SessionMessageRow {
    fn from(m: StoredMessage) -> Self {
        Self {
            ordinal: m.ordinal,
            superseded_by: m.superseded_by,
            created_at: m.created_at,
            message: m.message,
        }
    }
}

/// Per-job entry in [`TraceOverview`]: a [`JobSummary`] augmented with
/// the token aggregates the trace sidebar needs to render the
/// `↑in ↓out` chips before the user has clicked into the job. Token
/// fields are zeros when the underlying `QueryApi` has no `CostStore`
/// (CLI-style construction via `QueryApi::without_costs`) or when the
/// cost lookup errors — we'd rather show 0 than fail the whole page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceJobSummary {
    #[serde(flatten)]
    pub summary: JobSummary,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
}

/// Cheap session overview for the trace detail UI: the full
/// `session_messages` log + a list of job summaries, ordered
/// oldest-first to match the sidebar's `#1 #2 ...` numbering.
///
/// Compared to [`ReplayedConversation`] this drops the per-job
/// step/span tree — the client lazily fetches that via
/// [`QueryApi::load_job_trace`] on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceOverview {
    pub session_id: SessionId,
    /// Full transcript on an unconditional load; only rows with
    /// `ordinal > since_ordinal` on an incremental one.
    pub session_messages: Vec<SessionMessageRow>,
    pub jobs: Vec<TraceJobSummary>,
    /// Highest `superseded_by` marker in the session. Incremental
    /// pollers compare it to the value they last saw: a change means a
    /// compaction re-marked rows they already hold, so the cached
    /// prefix is stale and a full reload is required.
    pub supersede_watermark: Option<i64>,
}

/// Full step/span tree for a single job, served by
/// [`QueryApi::load_job_trace`]. Spans keep their original
/// `LlmCallInputs::Persisted` shape — clients slice the message log
/// from [`TraceOverview::session_messages`] themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTrace {
    pub job: Job,
    pub steps: Vec<ReplayStep>,
}

// ── QueryApi ────────────────────────────────────────────────────────

/// Read-only view onto the session/job/trace/cost stores.
///
/// `Arc<JobLifecycle>` rather than `Arc<dyn JobStore>` because the
/// lifecycle facade already wraps the store with the strong-typed
/// API the query layer wants (`list(Option<JobStatusKind>)` →
/// pre-sorted, status-filtered job lists).
pub struct QueryApi {
    sessions: Arc<dyn SessionStore>,
    jobs: Arc<JobLifecycle>,
    trace: Arc<dyn TraceStore>,
    /// Optional. Callers that only need lineage / replay / job listing
    /// (CLI `baybo trace list/show/export`, gateway `/v1/traces/{id}`)
    /// pass `None`; cost_summary then returns `Unsupported`.
    costs: Option<Arc<dyn CostStore>>,
}

impl QueryApi {
    pub fn new(
        sessions: Arc<dyn SessionStore>,
        jobs: Arc<JobLifecycle>,
        trace: Arc<dyn TraceStore>,
        costs: Arc<dyn CostStore>,
    ) -> Self {
        Self {
            sessions,
            jobs,
            trace,
            costs: Some(costs),
        }
    }

    /// Build a `QueryApi` without a `CostStore`. `cost_summary` calls
    /// return `QueryError::Unsupported` — used by the CLI trace
    /// commands which never touch cost data.
    pub fn without_costs(
        sessions: Arc<dyn SessionStore>,
        jobs: Arc<JobLifecycle>,
        trace: Arc<dyn TraceStore>,
    ) -> Self {
        Self {
            sessions,
            jobs,
            trace,
            costs: None,
        }
    }

    // ── 1. load_session ────────────────────────────────────────

    pub async fn load_session(&self, id: &SessionId) -> Result<Option<Session>> {
        Ok(self.sessions.get(id).await.map_err(SessionError::from)?)
    }

    // ── 2. list_jobs ───────────────────────────────────────────

    pub async fn list_jobs(
        &self,
        session_id: &SessionId,
        filter: JobFilter,
    ) -> Result<Vec<JobSummary>> {
        let summaries: Vec<JobSummary> = self
            .jobs
            .list_by_session(session_id, filter.status_kind)
            .await?
            .into_iter()
            .filter(|j| filter_matches(j, &filter))
            .map(|j| JobSummary::from_owned(&j))
            .collect();
        Ok(summaries)
    }

    // ── 3. load_job ────────────────────────────────────────────

    pub async fn load_job(&self, id: &JobId) -> Result<JobDetail> {
        let job = self
            .jobs
            .get(id)
            .await?
            .ok_or_else(|| QueryError::NotFound(format!("job {id}")))?;
        let steps = self
            .trace
            .list_steps_by_job(id)
            .await
            .map_err(TraceError::from)?
            .into_iter()
            .map(Step::from_row)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(JobDetail { job, steps })
    }

    // ── 4. load_step ───────────────────────────────────────────

    pub async fn load_step(&self, id: &StepId) -> Result<StepDetail> {
        let step = Step::from_row(
            self.trace
                .load_step(id)
                .await
                .map_err(TraceError::from)?
                .ok_or_else(|| QueryError::NotFound(format!("step {id}")))?,
        )?;
        let mut spans = self
            .trace
            .list_spans_by_step(id)
            .await
            .map_err(TraceError::from)?
            .into_iter()
            .map(Span::from_row)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // Span events are stored separately; fold them in so callers
        // get a fully self-contained `StepDetail`.
        self.attach_span_events(&mut spans).await?;
        Ok(StepDetail { step, spans })
    }

    /// Fill `events` on every span that has none inline, with one
    /// batched store query. Events are stored in their own table, so
    /// spans arrive with an empty `events` vec; fetching them per span
    /// is an O(spans) round-trip fan-out.
    async fn attach_span_events(&self, spans: &mut [Span]) -> Result<()> {
        let need: Vec<baybo_model::SpanId> = spans
            .iter()
            .filter(|s| s.events.is_empty())
            .map(|s| s.id)
            .collect();
        if need.is_empty() {
            return Ok(());
        }
        let mut events_by_span: HashMap<baybo_model::SpanId, Vec<SpanEvent>> = HashMap::new();
        for row in self
            .trace
            .list_span_events_for_spans(&need)
            .await
            .map_err(TraceError::from)?
        {
            let span_id = row.span_id;
            events_by_span
                .entry(span_id)
                .or_default()
                .push(SpanEvent::from_row(row)?);
        }
        for span in spans.iter_mut() {
            if span.events.is_empty()
                && let Some(evs) = events_by_span.remove(&span.id)
            {
                span.events = evs;
            }
        }
        Ok(())
    }

    /// Assemble one job's ordered `steps → spans (+ events)` tree from
    /// three batched store queries, regardless of tree size. The
    /// per-step / per-span round-trip fan-out this replaces cost
    /// O(steps + spans) pool checkouts per call.
    async fn load_step_tree_for_job(&self, job_id: &JobId) -> Result<Vec<ReplayStep>> {
        let mut steps = self
            .trace
            .list_steps_by_job(job_id)
            .await
            .map_err(TraceError::from)?
            .into_iter()
            .map(Step::from_row)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        steps.sort_by_key(|s| s.started_at);

        let mut spans = self
            .trace
            .list_spans_by_job(job_id)
            .await
            .map_err(TraceError::from)?
            .into_iter()
            .map(Span::from_row)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.attach_span_events(&mut spans).await?;

        let mut spans_by_step: HashMap<StepId, Vec<Span>> = HashMap::new();
        for span in spans {
            spans_by_step.entry(span.step_id).or_default().push(span);
        }
        Ok(steps
            .into_iter()
            .map(|step| {
                let spans = spans_by_step.remove(&step.id).unwrap_or_default();
                ReplayStep { step, spans }
            })
            .collect())
    }

    /// Per-session trace tally `(jobs, steps, spans)` for status
    /// surfaces — SQL counts per job, no step/span blob is ever
    /// materialised.
    pub async fn trace_counts(&self, session_id: &SessionId) -> Result<(usize, usize, usize)> {
        let jobs = self.jobs.list_by_session(session_id, None).await?;
        let mut steps = 0usize;
        let mut spans = 0usize;
        for job in &jobs {
            let (s, sp) = self
                .trace
                .trace_counts_by_job(&job.id)
                .await
                .map_err(TraceError::from)?;
            steps += s;
            spans += sp;
        }
        Ok((jobs.len(), steps, spans))
    }

    // ── 5. find_recoverable_jobs ───────────────────────────────

    pub async fn find_recoverable_jobs(&self) -> Result<Vec<Job>> {
        Ok(self.jobs.list_recoverable().await?)
    }

    // ── 6. list_active_subagents ───────────────────────────────

    pub async fn list_active_subagents(&self, session_id: &SessionId) -> Result<Vec<SessionId>> {
        let children = self
            .sessions
            .list_lineage_children(session_id)
            .await
            .map_err(SessionError::from)?;
        let mut out = Vec::new();
        for (child_id, kind) in children {
            if !matches!(kind, LineageKind::Subagent) {
                continue;
            }
            // "Active" = at least one non-terminal job in this child.
            let active = self
                .jobs
                .list_by_session(&child_id, None)
                .await?
                .iter()
                .any(|j| !j.status.is_terminal());
            if active {
                out.push(child_id);
            }
        }
        Ok(out)
    }

    // ── 7. lineage_tree ────────────────────────────────────────

    /// Walks descendants via `list_lineage_children`. Cycle protection
    /// caps recursion at `MAX_DEPTH = 32` — a runaway lineage tree
    /// indicates a bug and we'd rather truncate than stack overflow.
    pub async fn lineage_tree(&self, root_session_id: &SessionId) -> Result<LineageNode> {
        const MAX_DEPTH: usize = 32;
        // BFS to fetch every parent → children edge, then assemble the
        // tree in safe code. Two passes so the async fetch loop owns no
        // node references that could be invalidated by a later push.
        let mut edges: HashMap<SessionId, Vec<(SessionId, LineageKind)>> = HashMap::new();
        let mut frontier: Vec<(SessionId, usize)> = vec![(root_session_id.clone(), 0)];
        while let Some((parent_id, depth)) = frontier.pop() {
            if depth >= MAX_DEPTH {
                continue;
            }
            let kids = self
                .sessions
                .list_lineage_children(&parent_id)
                .await
                .map_err(SessionError::from)?;
            for (cid, kind) in kids {
                edges
                    .entry(parent_id.clone())
                    .or_default()
                    .push((cid.clone(), kind));
                frontier.push((cid, depth + 1));
            }
        }
        Ok(build_lineage_node(root_session_id.clone(), None, &edges))
    }

    // ── 8. cost_summary ────────────────────────────────────────

    pub async fn cost_summary(&self, scope: CostScope) -> Result<CostSummary> {
        let costs = self
            .costs
            .as_ref()
            .ok_or_else(|| QueryError::Unsupported("cost_summary requires a CostStore".into()))?;
        match scope {
            CostScope::TimeRange(range) => {
                Ok(costs.query_global(range).await.map_err(CostError::from)?)
            }
            CostScope::User { user_id, range } => Ok(costs
                .query_user_summary(&user_id, range)
                .await
                .map_err(CostError::from)?),
            CostScope::Session(sid) => {
                Ok(costs.query_session(&sid).await.map_err(CostError::from)?)
            }
            CostScope::Job(jid) => Ok(costs.query_job(&jid).await.map_err(CostError::from)?),
        }
    }

    // ── 10. list_session_summaries ─────────────────────────────

    /// List session summaries for the trace browser. Filters apply
    /// against `last_active` (time range), session id prefix, and the
    /// **latest** job's status kind. Sessions with zero jobs are
    /// dropped (a session with no trace is invisible to the browser).
    ///
    /// Cardinality: everything pre-pagination comes from the session
    /// scan plus one grouped job-stats query; the aggregates that need
    /// further store reads (full latest job, span counts, token
    /// totals) are computed for the returned page only. Request cost
    /// scales with page size, not total history.
    pub async fn list_session_summaries(
        &self,
        filter: SessionSummaryFilter,
        page: SessionSummaryPage,
    ) -> Result<SessionSummaryListing> {
        // Pull the candidate pool. `list_all` now returns every row;
        // the `filter.kind` retain below narrows to the requested
        // trigger/lineage class.
        let mut sessions = self.sessions.list_all().await.map_err(SessionError::from)?;

        if let Some(prefix) = filter.session_id_prefix.as_deref() {
            let needle = prefix.to_ascii_lowercase();
            sessions.retain(|s| s.id.as_str().to_ascii_lowercase().contains(&needle));
        }
        if let Some(since) = filter.since {
            sessions.retain(|s| s.last_active >= since);
        }
        if let Some(until) = filter.until {
            sessions.retain(|s| s.last_active < until);
        }
        if let Some(want_kind) = filter.kind {
            sessions.retain(|s| derive_session_kind(s) == want_kind);
        }

        let job_stats: HashMap<SessionId, baybo_store::SessionJobStats> = self
            .jobs
            .session_job_stats()
            .await?
            .into_iter()
            .map(|s| (s.session_id.clone(), s))
            .collect();

        // Latest-status filter + drop sessions with no jobs, from the
        // grouped stats alone — no per-session reads yet.
        sessions.retain(|s| match job_stats.get(&s.id) {
            None => false,
            Some(stats) => filter
                .status_kind
                .is_none_or(|want| stats.latest_status_kind == want.as_snake_case()),
        });

        // `SessionStore::list_all` already orders by `last_active`
        // DESC and every retain above preserves that order — paginate
        // before paying any per-session aggregate cost.
        let total = sessions.len();
        let start = page.offset.min(total);
        let end = if page.limit == 0 {
            total
        } else {
            start.saturating_add(page.limit).min(total)
        };

        // Page-item aggregates run concurrently across the (≤ page-size)
        // sessions; parallelism is bounded by the store's connection pool.
        let items =
            futures::future::try_join_all(sessions[start..end].iter().map(|session| {
                let session = session.clone();
                async move {
                    let jobs = self.jobs.list_by_session(&session.id, None).await?;
                    // Same (created_at, id) tiebreak as the grouped stats query, so
                    // the status shown always belongs to the job the filter judged.
                    let latest = jobs.iter().max_by_key(|j| (j.created_at, j.id));

                    let mut span_count = 0usize;
                    for job in &jobs {
                        span_count += self
                            .trace
                            .trace_counts_by_job(&job.id)
                            .await
                            .map_err(TraceError::from)?
                            .1;
                    }

                    let (
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        cache_creation_input_tokens,
                    ) = match self.costs.as_ref() {
                        Some(c) => match c.query_session(&session.id).await {
                            Ok(s) => (
                                s.total_input_tokens,
                                s.total_output_tokens,
                                s.total_cached_input_tokens,
                                s.total_cache_creation_input_tokens,
                            ),
                            Err(_) => (0, 0, 0, 0),
                        },
                        None => (0, 0, 0, 0),
                    };

                    Ok::<SessionSummary, QueryError>(SessionSummary {
                        session_id: session.id.clone(),
                        created_at: session.created_at,
                        last_active: session.last_active,
                        latest_job_status: latest.map(|j| j.status.clone()),
                        kind: derive_session_kind(&session),
                        job_count: jobs.len(),
                        span_count,
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        cache_creation_input_tokens,
                    })
                }
            }))
            .await?;

        Ok(SessionSummaryListing { items, total })
    }

    // ── 11. compute_analytics ──────────────────────────────────

    /// Aggregate cost records + session creations for the analytics
    /// dashboard. Iterates `cost_records` once for token / model
    /// breakdowns and `SessionStore::list_all` once for session-per-day
    /// counts. `Unsupported` if no `CostStore` was wired (CLI-style
    /// `QueryApi::without_costs` callers).
    pub async fn compute_analytics(&self, range: TimeRange) -> Result<AnalyticsSummary> {
        let costs = self.costs.as_ref().ok_or_else(|| {
            QueryError::Unsupported("compute_analytics requires a CostStore".into())
        })?;

        // Pre-build a contiguous YYYY-MM-DD bucket list (UTC) so days
        // with no activity still appear in the chart. Inclusive of the
        // `to` date so the day in progress (today) gets its own bucket.
        let mut day_index: HashMap<String, usize> = HashMap::new();
        let mut daily: Vec<AnalyticsDayBucket> = Vec::new();
        let mut cursor = range.from.date_naive();
        let last = range.to.date_naive();
        while cursor <= last {
            let key = cursor.format("%Y-%m-%d").to_string();
            day_index.insert(key.clone(), daily.len());
            daily.push(AnalyticsDayBucket {
                date: key,
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_usd: MicroUsd::ZERO,
                sessions_created: 0,
            });
            cursor = match cursor.succ_opt() {
                Some(d) => d,
                None => break,
            };
        }

        // All three breakdowns are grouped aggregates in SQL — the raw
        // records never cross the store boundary.
        use baybo_store::CostGroupKey;
        let day_buckets = costs
            .query_range_grouped(range.clone(), CostGroupKey::Day)
            .await
            .map_err(CostError::from)?;
        let model_groups = costs
            .query_range_grouped(range.clone(), CostGroupKey::Model)
            .await
            .map_err(CostError::from)?;
        let reason_groups = costs
            .query_range_grouped(range.clone(), CostGroupKey::Reason)
            .await
            .map_err(CostError::from)?;

        let mut total_input = 0usize;
        let mut total_output = 0usize;
        let mut total_cached = 0usize;
        let mut total_cache_create = 0usize;
        let mut total_cost = MicroUsd::ZERO;
        let mut total_records = 0usize;
        for b in &day_buckets {
            total_input += b.summary.total_input_tokens;
            total_output += b.summary.total_output_tokens;
            total_cached += b.summary.total_cached_input_tokens;
            total_cache_create += b.summary.total_cache_creation_input_tokens;
            total_cost += b.summary.total_cost_usd;
            total_records += b.summary.record_count;
            if let Some(&i) = day_index.get(&b.key) {
                let bucket = &mut daily[i];
                bucket.input_tokens = b.summary.total_input_tokens;
                bucket.output_tokens = b.summary.total_output_tokens;
                bucket.cached_input_tokens = b.summary.total_cached_input_tokens;
                bucket.cache_creation_input_tokens = b.summary.total_cache_creation_input_tokens;
                bucket.cost_usd = b.summary.total_cost_usd;
            }
        }

        // sessions_created per day: a flat created_at projection —
        // no session blobs are decoded.
        for created_at in self
            .sessions
            .session_created_times(range.from, range.to)
            .await
            .map_err(SessionError::from)?
        {
            let day_key = created_at.date_naive().format("%Y-%m-%d").to_string();
            if let Some(&i) = day_index.get(&day_key) {
                daily[i].sessions_created += 1;
            }
        }

        let mut model_buckets: Vec<AnalyticsModelBucket> = model_groups
            .into_iter()
            .map(|b| AnalyticsModelBucket {
                model: b.key,
                input_tokens: b.summary.total_input_tokens,
                output_tokens: b.summary.total_output_tokens,
                cached_input_tokens: b.summary.total_cached_input_tokens,
                cache_creation_input_tokens: b.summary.total_cache_creation_input_tokens,
                cost_usd: b.summary.total_cost_usd,
                call_count: b.summary.record_count,
            })
            .collect();
        model_buckets.sort_by(|a, b| {
            (b.input_tokens + b.output_tokens).cmp(&(a.input_tokens + a.output_tokens))
        });

        // Distinct stored tokens can parse to the same `CallReason`
        // (NULL and the explicit default both fold to the default
        // variant), so re-merge after parsing.
        let mut by_reason: HashMap<CallReason, AnalyticsReasonBucket> = HashMap::new();
        for b in reason_groups {
            let reason = CallReason::parse(&b.key).unwrap_or_default();
            let entry = by_reason
                .entry(reason.clone())
                .or_insert_with(|| AnalyticsReasonBucket {
                    reason,
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cost_usd: MicroUsd::ZERO,
                    call_count: 0,
                });
            entry.input_tokens += b.summary.total_input_tokens;
            entry.output_tokens += b.summary.total_output_tokens;
            entry.cached_input_tokens += b.summary.total_cached_input_tokens;
            entry.cache_creation_input_tokens += b.summary.total_cache_creation_input_tokens;
            entry.cost_usd += b.summary.total_cost_usd;
            entry.call_count += b.summary.record_count;
        }
        let mut reason_buckets: Vec<AnalyticsReasonBucket> = by_reason.into_values().collect();
        reason_buckets.sort_by(|a, b| {
            (b.input_tokens + b.output_tokens).cmp(&(a.input_tokens + a.output_tokens))
        });

        Ok(AnalyticsSummary {
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cached_input_tokens: total_cached,
            total_cache_creation_input_tokens: total_cache_create,
            total_cost_usd: total_cost,
            total_record_count: total_records,
            daily,
            by_model: model_buckets,
            by_reason: reason_buckets,
        })
    }

    // ── 9. replay ──────────────────────────────────────────────

    pub async fn replay(
        &self,
        session_id: &SessionId,
        until_step_id: Option<StepId>,
    ) -> Result<ReplayedConversation> {
        // Full jobs, oldest-first for replay.
        let mut full_jobs = self.jobs.list_by_session(session_id, None).await?;
        full_jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        // The truncation target's owning job comes from the step row
        // itself — one point lookup instead of scanning every job's
        // step list.
        let truncate_after_job: Option<JobId> = match until_step_id.as_ref() {
            None => None,
            Some(target) => self
                .trace
                .load_step(target)
                .await
                .map_err(TraceError::from)?
                .map(Step::from_row)
                .transpose()?
                .map(|s| s.job_id),
        };

        let mut jobs = Vec::with_capacity(full_jobs.len());
        for job in full_jobs {
            let job_id = job.id;
            let mut step_blocks = self.load_step_tree_for_job(&job_id).await?;
            if let Some(target) = until_step_id
                && let Some(pos) = step_blocks.iter().position(|b| b.step.id == target)
            {
                step_blocks.truncate(pos + 1);
            }
            jobs.push(ReplayJob {
                job,
                steps: step_blocks,
            });
            if Some(job_id) == truncate_after_job {
                break;
            }
        }

        // Hydrate transcript-backed LLM inputs and tool outputs into their
        // legacy inline API shapes. The session_messages log is read once per
        // session and reused for every reference.
        self.hydrate_persisted_trace_data(session_id, &mut jobs)
            .await?;

        Ok(ReplayedConversation {
            session_id: session_id.clone(),
            jobs,
        })
    }

    // ── 12. load_trace_overview ────────────────────────────────

    /// Trace detail page's first call: returns the session message
    /// log + job summaries, **without** any step/span data. Cheap.
    /// The client lazily fetches each job's tree via
    /// [`Self::load_job_trace`] when the user selects it.
    ///
    /// This split exists because the old single-shot `replay`
    /// inlined the whole transcript into every `LlmCall` span — for a
    /// session with N jobs × S spans the response payload was
    /// O(N · S · message_count), even though storage is already
    /// O(N · S) thanks to the `LlmCallInputs::Persisted` ordinal
    /// indirection. Returning the message log once and letting the
    /// client hydrate slices keeps the wire payload linear in
    /// `message_count + span_count`.
    /// `since_ordinal`: when set, `session_messages` carries only rows
    /// with a greater ordinal — the caller already holds the prefix and
    /// re-validates it against [`TraceOverview::supersede_watermark`].
    pub async fn load_trace_overview(
        &self,
        session_id: &SessionId,
        since_ordinal: Option<i64>,
    ) -> Result<TraceOverview> {
        // Re-sort oldest-first to match the trace sidebar's job
        // numbering (`#1, #2, ...` from earliest to latest).
        let mut summaries = self.list_jobs(session_id, JobFilter::default()).await?;
        summaries.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        let (session_messages, supersede_watermark): (Vec<SessionMessageRow>, Option<i64>) =
            match since_ordinal {
                // The incremental page holds only a suffix, so the watermark
                // needs its own (indexed MAX) query.
                Some(since) => {
                    let rows = self
                        .sessions
                        .load_session_messages_with_supersede_since(session_id, since)
                        .await
                        .map_err(SessionError::from)?;
                    let watermark = self
                        .sessions
                        .supersede_watermark(session_id)
                        .await
                        .map_err(SessionError::from)?;
                    (
                        rows.into_iter().map(SessionMessageRow::from).collect(),
                        watermark,
                    )
                }
                // The full load already carries every `superseded_by` marker —
                // derive the watermark instead of re-scanning the partition.
                None => {
                    let rows: Vec<SessionMessageRow> = self
                        .sessions
                        .load_session_messages_with_supersede(session_id)
                        .await
                        .map_err(SessionError::from)?
                        .into_iter()
                        .map(SessionMessageRow::from)
                        .collect();
                    let watermark = rows.iter().filter_map(|r| r.superseded_by).max();
                    (rows, watermark)
                }
            };

        // Per-job token aggregates power the sidebar's `↑in ↓out`
        // chips: one grouped cost query for the whole session. A cost
        // failure degrades to zeroed chips rather than failing the
        // overview.
        let costs_by_job: HashMap<String, CostSummary> = match self.costs.as_ref() {
            Some(c) => match c.query_session_by_job(session_id).await {
                Ok(buckets) => buckets.into_iter().map(|b| (b.key, b.summary)).collect(),
                Err(_) => HashMap::new(),
            },
            None => HashMap::new(),
        };
        let jobs = summaries
            .into_iter()
            .map(|summary| {
                let c = costs_by_job.get(&summary.id.to_string());
                TraceJobSummary {
                    input_tokens: c.map_or(0, |c| c.total_input_tokens),
                    output_tokens: c.map_or(0, |c| c.total_output_tokens),
                    cached_input_tokens: c.map_or(0, |c| c.total_cached_input_tokens),
                    cache_creation_input_tokens: c
                        .map_or(0, |c| c.total_cache_creation_input_tokens),
                    summary,
                }
            })
            .collect();

        Ok(TraceOverview {
            session_id: session_id.clone(),
            session_messages,
            jobs,
            supersede_watermark,
        })
    }

    // ── 13. load_job_trace ─────────────────────────────────────

    /// Per-job follow-up to [`Self::load_trace_overview`]: returns
    /// one job's full `steps → spans → events` tree. `Persisted`
    /// `input_messages` references stay as ordinal pointers; the
    /// client resolves them against the message log it already
    /// received from the overview call.
    pub async fn load_job_trace(&self, job_id: &JobId) -> Result<JobTrace> {
        let job = self
            .jobs
            .get(job_id)
            .await?
            .ok_or_else(|| QueryError::NotFound(format!("job {job_id}")))?;
        let step_blocks = self.load_step_tree_for_job(job_id).await?;
        Ok(JobTrace {
            job,
            steps: step_blocks,
        })
    }

    /// Walk every span in `jobs`, hydrating both persisted LLM input slices and
    /// transcript-backed tool outputs into their legacy inline API shapes.
    /// Skips the work entirely if no span needs hydration; reads the log once
    /// when at least one does.
    /// `log_session_id` is the session whose log to read — the replayed
    /// session itself, since every session records its `Persisted` refs
    /// against its own transcript.
    async fn hydrate_persisted_trace_data(
        &self,
        log_session_id: &SessionId,
        jobs: &mut [ReplayJob],
    ) -> Result<()> {
        use baybo_trace::{LlmCallInputs, SpanKind, ToolCallOutput};

        let any_persisted = jobs.iter().any(|j| {
            j.steps.iter().any(|s| {
                s.spans.iter().any(|sp| {
                    matches!(
                        &sp.kind,
                        SpanKind::LlmCall { begin, .. } if begin.input_messages.is_persisted()
                    ) || matches!(
                        &sp.kind,
                        SpanKind::ToolCall { result: Some(result), .. }
                            if result.output.is_persisted()
                    )
                })
            })
        });
        if !any_persisted {
            return Ok(());
        }

        let log = self
            .sessions
            .load_session_messages_with_supersede(log_session_id)
            .await
            .map_err(SessionError::from)?;

        for job in jobs.iter_mut() {
            for step in job.steps.iter_mut() {
                for span in step.spans.iter_mut() {
                    let span_started_at = span.started_at;
                    if let SpanKind::LlmCall { begin, .. } = &mut span.kind
                        && let LlmCallInputs::Persisted {
                            last_ordinal,
                            prefix_len,
                            suffix,
                        } = &begin.input_messages
                    {
                        let last = *last_ordinal;
                        let expected_prefix = *prefix_len;
                        let suffix = suffix.clone();
                        let candidates: Vec<&StoredMessage> = log
                            .iter()
                            .filter(|m| {
                                m.ordinal <= last
                                    && m.superseded_by.map(|s| s > last).unwrap_or(true)
                            })
                            .collect();
                        // Detect ordinal collisions across session
                        // lifetimes: if any candidate row was written
                        // after the span started, the current
                        // session_messages log doesn't represent the
                        // transcript this span saw (the parent session
                        // was reset and the ordinals were reused). In
                        // that case, surface an empty input rather
                        // than misleading content from a different
                        // epoch.
                        let mismatch = candidates.iter().any(|m| m.created_at > span_started_at);
                        let hydrated: Vec<baybo_model::ChatMessage> = if mismatch {
                            tracing::warn!(
                                log_session_id = %log_session_id,
                                span_id = %span.id,
                                last_ordinal = last,
                                "session_messages epoch mismatch — span predates current log; \
                                 returning empty input"
                            );
                            Vec::new()
                        } else {
                            // Active prefix (by ordinal) then the inline
                            // suffix (framing / sub-loop turns not in the
                            // log) — together the exact slice the LLM saw.
                            let mut active: Vec<baybo_model::ChatMessage> =
                                candidates.iter().map(|m| m.message.clone()).collect();
                            // Tripwire: the reconstructed prefix count must
                            // match what the writer recorded. A divergence
                            // means the log drifted under the reference (a
                            // `superseded_by` bug, a deleted row, or a
                            // read/write filter divergence) — flag it with a
                            // visible marker rather than returning a
                            // plausible-but-wrong slice silently.
                            if active.len() != expected_prefix {
                                tracing::warn!(
                                    log_session_id = %log_session_id,
                                    span_id = %span.id,
                                    last_ordinal = last,
                                    expected = expected_prefix,
                                    reconstructed = active.len(),
                                    "trace input reconstruction count mismatch — \
                                     session_messages drifted under the Persisted reference; \
                                     flagging the rehydrated input"
                                );
                                active.insert(
                                    0,
                                    reconstruction_warning(expected_prefix, active.len()),
                                );
                            }
                            active.extend(suffix);
                            active
                        };
                        begin.input_messages = LlmCallInputs::Inline(hydrated);
                    }

                    if let SpanKind::ToolCall {
                        result: Some(result),
                        ..
                    } = &mut span.kind
                        && let ToolCallOutput::Persisted(reference) = &result.output
                    {
                        // The reference is keyed on the call's `tool_use_id`, so
                        // find the (unsuperseded-or-not) row appended for it
                        // after the span opened. `resolve` only succeeds for the
                        // row that actually carries the block.
                        let resolved = log
                            .iter()
                            .filter(|message| message.created_at >= span_started_at)
                            .find_map(|message| reference.resolve(&message.message).ok());
                        result.output = ToolCallOutput::Inline(match resolved {
                            Some(value) => value,
                            None => {
                                tracing::warn!(
                                    log_session_id = %log_session_id,
                                    span_id = %span.id,
                                    tool_use_id = %reference.tool_use_id,
                                    "tool trace output reconstruction failed"
                                );
                                tool_output_reconstruction_warning(
                                    &reference.tool_use_id,
                                    "no transcript ToolResult found for this tool_use_id",
                                )
                            }
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

fn tool_output_reconstruction_warning(tool_use_id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "trace_reconstruction_error",
        "tool_use_id": tool_use_id,
        "error": reason,
    })
}

/// Visible in-band marker prepended to a rehydrated input whose
/// reconstructed prefix count didn't match the span's `prefix_len`
/// tripwire. A `Role::System` message so trace viewers render it
/// distinctly and `source == 'user'` prompt-detection never picks it up.
fn reconstruction_warning(expected: usize, reconstructed: usize) -> baybo_model::ChatMessage {
    baybo_model::ChatMessage::system(vec![baybo_model::ContentBlock::Text(format!(
        "⚠️ trace reconstruction inconsistent: expected {expected} prefix message(s) from \
         session_messages, reconstructed {reconstructed}. The log drifted under this span's \
         ordinal reference — the input shown may be incomplete or wrong."
    ))])
}

fn filter_matches(j: &Job, f: &JobFilter) -> bool {
    if let Some(s) = f.since
        && j.created_at < s
    {
        return false;
    }
    if let Some(u) = f.until
        && j.created_at >= u
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_cost::test_support::MemoryCostStore;
    use baybo_job::JobInput;
    use baybo_job::test_support::MemoryJobStore;
    use baybo_model::{ChannelType, ContentBlock, TriggerKind, TriggerSource};
    use baybo_session::SessionStore;
    use baybo_store::JobStore as _;
    use baybo_trace::test_support::MemoryTraceStore;
    use std::sync::Arc;

    fn user_input() -> JobInput {
        JobInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    /// Minimal in-memory `SessionStore` for query-API tests. The
    /// `session_messages` log is a real append-only vector so the
    /// hydration test can drive the same supersede semantics the
    /// sqlite backend implements.
    #[derive(Default)]
    struct MemSessionStore {
        sessions: parking_lot::Mutex<HashMap<SessionId, Session>>,
        children: parking_lot::Mutex<HashMap<SessionId, Vec<(SessionId, LineageKind)>>>,
        messages: parking_lot::Mutex<HashMap<SessionId, Vec<StoredMessage>>>,
        source_event_ordinals: parking_lot::Mutex<HashMap<(SessionId, String), i64>>,
        read_cursors: parking_lot::Mutex<HashMap<SessionId, i64>>,
    }

    #[async_trait::async_trait]
    impl SessionStore for MemSessionStore {
        async fn get(
            &self,
            id: &SessionId,
        ) -> std::result::Result<Option<Session>, baybo_store::StorageError> {
            Ok(self.sessions.lock().get(id).cloned())
        }
        async fn save(
            &self,
            session: &Session,
        ) -> std::result::Result<(), baybo_store::StorageError> {
            self.sessions
                .lock()
                .insert(session.id.clone(), session.clone());
            Ok(())
        }
        async fn set_hidden(
            &self,
            id: &SessionId,
            hidden: bool,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            let mut data = self.sessions.lock();
            match data.get_mut(id) {
                Some(s) => {
                    s.hidden = hidden;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn set_last_llm(
            &self,
            id: &SessionId,
            llm: Option<&baybo_model::LlmEntryName>,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            let mut data = self.sessions.lock();
            match data.get_mut(id) {
                Some(s) => {
                    s.state.last_llm = llm.cloned();
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn set_pinned(
            &self,
            id: &SessionId,
            pinned: bool,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            let mut data = self.sessions.lock();
            match data.get_mut(id) {
                Some(s) => {
                    s.pinned = pinned;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn set_archived(
            &self,
            id: &SessionId,
            archived: bool,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            let mut data = self.sessions.lock();
            match data.get_mut(id) {
                Some(s) => {
                    s.archived = archived;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn set_folder(
            &self,
            id: &SessionId,
            folder_id: Option<&baybo_model::FolderId>,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            let mut data = self.sessions.lock();
            match data.get_mut(id) {
                Some(s) => {
                    s.folder_id = folder_id.cloned();
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn set_read_cursor(
            &self,
            id: &SessionId,
            ordinal: i64,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            if !self.sessions.lock().contains_key(id) {
                return Ok(false);
            }
            let mut cursors = self.read_cursors.lock();
            let entry = cursors.entry(id.clone()).or_insert(ordinal);
            if ordinal > *entry {
                *entry = ordinal;
            }
            Ok(true)
        }
        async fn set_title(
            &self,
            id: &SessionId,
            title: Option<&str>,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            let mut data = self.sessions.lock();
            match data.get_mut(id) {
                Some(s) => {
                    s.title = title.map(|t| t.to_string());
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn delete(
            &self,
            _id: &SessionId,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            Ok(true)
        }
        async fn list_expired(
            &self,
            _before: DateTime<Utc>,
        ) -> std::result::Result<Vec<SessionId>, baybo_store::StorageError> {
            Ok(Vec::new())
        }
        async fn list_all(&self) -> std::result::Result<Vec<Session>, baybo_store::StorageError> {
            // Contract: newest `last_active` first (matches the sqlite
            // backend; `list_session_summaries` pagination relies on it).
            let mut out: Vec<Session> = self.sessions.lock().values().cloned().collect();
            out.sort_by_key(|s| std::cmp::Reverse(s.last_active));
            Ok(out)
        }
        async fn list_lineage_children(
            &self,
            parent: &SessionId,
        ) -> std::result::Result<Vec<(SessionId, LineageKind)>, baybo_store::StorageError> {
            Ok(self
                .children
                .lock()
                .get(parent)
                .cloned()
                .unwrap_or_default())
        }
        async fn append_session_message(
            &self,
            id: &SessionId,
            message: &baybo_model::ChatMessage,
        ) -> std::result::Result<i64, baybo_store::StorageError> {
            let mut guard = self.messages.lock();
            let log = guard.entry(id.clone()).or_default();
            let ordinal: i64 = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
            log.push(StoredMessage {
                ordinal,
                superseded_by: None,
                created_at: chrono::Utc::now(),
                message: message.clone(),
            });
            Ok(ordinal)
        }
        async fn append_session_message_idempotent(
            &self,
            id: &SessionId,
            source_event_id: &str,
            message: &baybo_model::ChatMessage,
        ) -> std::result::Result<baybo_store::SessionMessageAppendOutcome, baybo_store::StorageError>
        {
            if let Some(ordinal) = self
                .source_event_ordinals
                .lock()
                .get(&(id.clone(), source_event_id.to_string()))
                .copied()
            {
                return Ok(baybo_store::SessionMessageAppendOutcome::Existing { ordinal });
            }
            let ordinal = self.append_session_message(id, message).await?;
            self.source_event_ordinals
                .lock()
                .insert((id.clone(), source_event_id.to_string()), ordinal);
            Ok(baybo_store::SessionMessageAppendOutcome::Inserted { ordinal })
        }
        async fn find_message_ordinal_by_source_event_id(
            &self,
            id: &SessionId,
            source_event_id: &str,
        ) -> std::result::Result<Option<i64>, baybo_store::StorageError> {
            Ok(self
                .source_event_ordinals
                .lock()
                .get(&(id.clone(), source_event_id.to_string()))
                .copied())
        }
        async fn append_control_event(
            &self,
            _id: &SessionId,
            _after_ordinal: i64,
            _kind: baybo_model::ControlEventKind,
            _text: &str,
            _created_at: DateTime<Utc>,
        ) -> std::result::Result<i64, baybo_store::StorageError> {
            Ok(0)
        }
        async fn list_control_events(
            &self,
            _id: &SessionId,
        ) -> std::result::Result<Vec<baybo_model::ControlEvent>, baybo_store::StorageError>
        {
            Ok(Vec::new())
        }
        async fn list_control_events_in_range(
            &self,
            _id: &SessionId,
            _lower: i64,
            _upper: i64,
        ) -> std::result::Result<Vec<baybo_model::ControlEvent>, baybo_store::StorageError>
        {
            Ok(Vec::new())
        }
        async fn apply_session_compaction(
            &self,
            id: &SessionId,
            new_active: &[baybo_model::ChatMessage],
        ) -> std::result::Result<(), baybo_store::StorageError> {
            let mut guard = self.messages.lock();
            let log = guard.entry(id.clone()).or_default();
            let next_ordinal = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
            for entry in log.iter_mut() {
                if entry.superseded_by.is_none() {
                    entry.superseded_by = Some(next_ordinal);
                }
            }
            let stamp = chrono::Utc::now();
            for (offset, msg) in new_active.iter().enumerate() {
                log.push(StoredMessage {
                    ordinal: next_ordinal + offset as i64,
                    superseded_by: None,
                    created_at: stamp,
                    message: msg.clone(),
                });
            }
            Ok(())
        }
        async fn load_active_session_messages(
            &self,
            id: &SessionId,
        ) -> std::result::Result<Vec<baybo_model::ChatMessage>, baybo_store::StorageError> {
            Ok(self
                .messages
                .lock()
                .get(id)
                .map(|log| {
                    log.iter()
                        .filter(|m| m.superseded_by.is_none())
                        .map(|m| m.message.clone())
                        .collect()
                })
                .unwrap_or_default())
        }
        async fn latest_session_ordinal(
            &self,
            id: &SessionId,
        ) -> std::result::Result<Option<i64>, baybo_store::StorageError> {
            Ok(self
                .messages
                .lock()
                .get(id)
                .and_then(|log| log.iter().map(|m| m.ordinal).max()))
        }
        async fn load_session_messages_with_supersede(
            &self,
            id: &SessionId,
        ) -> std::result::Result<Vec<StoredMessage>, baybo_store::StorageError> {
            Ok(self.messages.lock().get(id).cloned().unwrap_or_default())
        }
        async fn load_session_messages_with_supersede_since(
            &self,
            id: &SessionId,
            after_ordinal: i64,
        ) -> std::result::Result<Vec<StoredMessage>, baybo_store::StorageError> {
            Ok(self
                .messages
                .lock()
                .get(id)
                .map(|rows| {
                    rows.iter()
                        .filter(|m| m.ordinal > after_ordinal)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default())
        }
        async fn supersede_watermark(
            &self,
            id: &SessionId,
        ) -> std::result::Result<Option<i64>, baybo_store::StorageError> {
            Ok(self
                .messages
                .lock()
                .get(id)
                .and_then(|rows| rows.iter().filter_map(|m| m.superseded_by).max()))
        }
        async fn session_created_times(
            &self,
            from: DateTime<Utc>,
            to: DateTime<Utc>,
        ) -> std::result::Result<Vec<DateTime<Utc>>, baybo_store::StorageError> {
            Ok(self
                .sessions
                .lock()
                .values()
                .map(|s| s.created_at)
                .filter(|t| *t >= from && *t < to)
                .collect())
        }
        async fn last_user_messages(
            &self,
            session_ids: &[SessionId],
        ) -> std::result::Result<
            Vec<(SessionId, DateTime<Utc>, baybo_model::ChatMessage)>,
            baybo_store::StorageError,
        > {
            let msgs = self.messages.lock();
            Ok(session_ids
                .iter()
                .filter_map(|id| {
                    msgs.get(id).and_then(|rows| {
                        rows.iter()
                            .filter(|m| m.superseded_by.is_none() && m.message.from_user())
                            .max_by_key(|m| m.ordinal)
                            .map(|m| (id.clone(), m.created_at, m.message.clone()))
                    })
                })
                .collect())
        }
        async fn active_tails(
            &self,
            session_ids: &[SessionId],
            limit: usize,
        ) -> std::result::Result<
            Vec<(SessionId, i64, DateTime<Utc>, baybo_model::ChatMessage)>,
            baybo_store::StorageError,
        > {
            let msgs = self.messages.lock();
            let mut out = Vec::new();
            for id in session_ids {
                if let Some(rows) = msgs.get(id) {
                    let mut active: Vec<_> =
                        rows.iter().filter(|m| m.superseded_by.is_none()).collect();
                    active.sort_by_key(|m| m.ordinal);
                    let start = active.len().saturating_sub(limit);
                    out.extend(
                        active[start..]
                            .iter()
                            .map(|m| (id.clone(), m.ordinal, m.created_at, m.message.clone())),
                    );
                }
            }
            Ok(out)
        }
        async fn unread_scan(
            &self,
            session_ids: &[SessionId],
            limit: usize,
        ) -> std::result::Result<
            Vec<(SessionId, baybo_model::ChatMessage)>,
            baybo_store::StorageError,
        > {
            let msgs = self.messages.lock();
            let cursors = self.read_cursors.lock();
            let mut out = Vec::new();
            for id in session_ids {
                let cursor = cursors.get(id).copied().unwrap_or(-1);
                if let Some(rows) = msgs.get(id) {
                    let mut active: Vec<_> = rows
                        .iter()
                        .filter(|m| m.superseded_by.is_none() && m.ordinal > cursor)
                        .collect();
                    active.sort_by_key(|m| m.ordinal);
                    out.extend(
                        active
                            .into_iter()
                            .take(limit)
                            .map(|m| (id.clone(), m.message.clone())),
                    );
                }
            }
            Ok(out)
        }
        async fn session_titles(
            &self,
            session_ids: &[SessionId],
        ) -> std::result::Result<Vec<(SessionId, Option<String>)>, baybo_store::StorageError>
        {
            let sessions = self.sessions.lock();
            Ok(session_ids
                .iter()
                .filter_map(|id| sessions.get(id).map(|s| (id.clone(), s.title.clone())))
                .collect())
        }
        async fn session_channels(
            &self,
            session_ids: &[SessionId],
        ) -> std::result::Result<Vec<(SessionId, String)>, baybo_store::StorageError> {
            let sessions = self.sessions.lock();
            Ok(session_ids
                .iter()
                .filter_map(|id| {
                    sessions
                        .get(id)
                        .map(|s| (id.clone(), s.channel.as_str().to_string()))
                })
                .collect())
        }
        async fn touch_last_active(
            &self,
            id: &SessionId,
            now: DateTime<Utc>,
        ) -> std::result::Result<bool, baybo_store::StorageError> {
            let mut sessions = self.sessions.lock();
            match sessions.get_mut(id) {
                Some(s) => {
                    s.last_active = now;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn count_sessions(&self) -> std::result::Result<usize, baybo_store::StorageError> {
            Ok(self.sessions.lock().len())
        }
        async fn active_index_of_ordinal(
            &self,
            id: &SessionId,
            ordinal: i64,
        ) -> std::result::Result<Option<usize>, baybo_store::StorageError> {
            Ok(self.messages.lock().get(id).and_then(|log| {
                log.iter()
                    .filter(|m| m.superseded_by.is_none())
                    .position(|m| m.ordinal == ordinal)
            }))
        }
        async fn count_active_messages(
            &self,
            id: &SessionId,
        ) -> std::result::Result<usize, baybo_store::StorageError> {
            Ok(self
                .messages
                .lock()
                .get(id)
                .map(|log| log.iter().filter(|m| m.superseded_by.is_none()).count())
                .unwrap_or(0))
        }
        async fn load_active_session_messages_up_to(
            &self,
            id: &SessionId,
            up_to_ordinal: i64,
        ) -> std::result::Result<Vec<baybo_model::ChatMessage>, baybo_store::StorageError> {
            Ok(self
                .messages
                .lock()
                .get(id)
                .map(|log| {
                    log.iter()
                        .filter(|m| m.superseded_by.is_none() && m.ordinal <= up_to_ordinal)
                        .map(|m| m.message.clone())
                        .collect()
                })
                .unwrap_or_default())
        }
        async fn load_active_session_messages_tail(
            &self,
            id: &SessionId,
            before_ordinal: Option<i64>,
            limit: usize,
        ) -> std::result::Result<
            Vec<(i64, chrono::DateTime<chrono::Utc>, baybo_model::ChatMessage)>,
            baybo_store::StorageError,
        > {
            if limit == 0 {
                return Ok(Vec::new());
            }
            Ok(self
                .messages
                .lock()
                .get(id)
                .map(|log| {
                    let active: Vec<&StoredMessage> = log
                        .iter()
                        .filter(|m| {
                            m.superseded_by.is_none()
                                && before_ordinal.is_none_or(|b| m.ordinal < b)
                        })
                        .collect();
                    let skip = active.len().saturating_sub(limit);
                    active
                        .into_iter()
                        .skip(skip)
                        .map(|m| (m.ordinal, m.created_at, m.message.clone()))
                        .collect()
                })
                .unwrap_or_default())
        }
        async fn load_active_session_messages_since(
            &self,
            id: &SessionId,
            after_ordinal: i64,
            limit: usize,
        ) -> std::result::Result<
            Vec<(i64, chrono::DateTime<chrono::Utc>, baybo_model::ChatMessage)>,
            baybo_store::StorageError,
        > {
            if limit == 0 {
                return Ok(Vec::new());
            }
            Ok(self
                .messages
                .lock()
                .get(id)
                .map(|log| {
                    log.iter()
                        .filter(|m| m.superseded_by.is_none() && m.ordinal > after_ordinal)
                        .take(limit)
                        .map(|m| (m.ordinal, m.created_at, m.message.clone()))
                        .collect()
                })
                .unwrap_or_default())
        }
        async fn find_message_ordinal_by_platform_msg_id(
            &self,
            id: &SessionId,
            platform_msg_id: &str,
        ) -> std::result::Result<Option<i64>, baybo_store::StorageError> {
            if platform_msg_id.is_empty() {
                return Ok(None);
            }
            Ok(self.messages.lock().get(id).and_then(|log| {
                log.iter()
                    .filter(|m| m.message.platform_msg_id() == platform_msg_id)
                    .max_by_key(|m| m.ordinal)
                    .map(|m| m.ordinal)
            }))
        }
    }

    fn make_session(id: &str) -> Session {
        let id = SessionId::from(id);
        Session {
            id: id.clone(),
            user: baybo_model::User {
                id: "u1".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: Default::default(),
            root_session_id: id,
            trigger: TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    fn make_query_api(sessions: Arc<dyn SessionStore>) -> QueryApi {
        let job_store = Arc::new(MemoryJobStore::new());
        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let cost_store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let lifecycle = Arc::new(JobLifecycle::new(job_store));
        QueryApi::new(sessions, lifecycle, trace_store, cost_store)
    }

    /// A `TraceStore` whose reads all fail with a storage error. Used to
    /// prove a trace-store failure surfaces as `QueryError::Trace`, not
    /// `QueryError::Session` — a regression guard against the blanket
    /// `From<StorageError>` that once funnelled every store's failure into
    /// the session variant.
    struct FailingTraceStore;

    #[async_trait::async_trait]
    impl TraceStore for FailingTraceStore {
        async fn save_step(&self, _: &baybo_store::StepRow) -> baybo_store::trace::Result<()> {
            Ok(())
        }
        async fn load_step(
            &self,
            _: &StepId,
        ) -> baybo_store::trace::Result<Option<baybo_store::StepRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn list_steps_by_job(
            &self,
            _: &JobId,
        ) -> baybo_store::trace::Result<Vec<baybo_store::StepRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn list_unfinished_steps(
            &self,
        ) -> baybo_store::trace::Result<Vec<baybo_store::StepRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn save_span(&self, _: &baybo_store::SpanRow) -> baybo_store::trace::Result<()> {
            Ok(())
        }
        async fn load_span(
            &self,
            _: &baybo_model::SpanId,
        ) -> baybo_store::trace::Result<Option<baybo_store::SpanRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn list_spans_by_step(
            &self,
            _: &StepId,
        ) -> baybo_store::trace::Result<Vec<baybo_store::SpanRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn trace_counts_by_job(
            &self,
            _: &JobId,
        ) -> baybo_store::trace::Result<(usize, usize)> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn list_spans_by_job(
            &self,
            _: &JobId,
        ) -> baybo_store::trace::Result<Vec<baybo_store::SpanRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn list_span_events_for_spans(
            &self,
            _: &[baybo_model::SpanId],
        ) -> baybo_store::trace::Result<Vec<baybo_store::SpanEventRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
        async fn append_span_event(
            &self,
            _: &baybo_store::SpanEventRow,
        ) -> baybo_store::trace::Result<()> {
            Ok(())
        }
        async fn list_span_events(
            &self,
            _: &baybo_model::SpanId,
        ) -> baybo_store::trace::Result<Vec<baybo_store::SpanEventRow>> {
            Err(baybo_store::StorageError::Storage("boom".into()))
        }
    }

    #[tokio::test]
    async fn trace_store_failure_surfaces_as_trace_error() {
        let sessions: Arc<dyn SessionStore> = Arc::new(MemSessionStore::default());
        let lifecycle = Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new())));
        let trace: Arc<dyn TraceStore> = Arc::new(FailingTraceStore);
        let cost: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let api = QueryApi::new(sessions, lifecycle, trace, cost);

        let err = api.load_step(&StepId::new()).await.unwrap_err();
        assert!(
            matches!(err, QueryError::Trace(_)),
            "a trace-store failure must surface as QueryError::Trace, got {err:?}"
        );
    }

    #[tokio::test]
    async fn load_session_returns_session() {
        let store = Arc::new(MemSessionStore::default());
        let s = make_session("cli-1");
        store.save(&s).await.unwrap();
        let api = make_query_api(store);
        let loaded = api.load_session(&s.id).await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, s.id);
    }

    #[tokio::test]
    async fn load_session_missing_returns_none() {
        let store = Arc::new(MemSessionStore::default());
        let api = make_query_api(store);
        assert!(
            api.load_session(&SessionId::from("nope"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn find_recoverable_jobs_filters_to_non_terminal() {
        let store = Arc::new(MemSessionStore::default());
        let job_store = Arc::new(MemoryJobStore::new());
        let lifecycle = Arc::new(JobLifecycle::new(job_store.clone()));
        let api = QueryApi::new(
            store,
            lifecycle.clone(),
            Arc::new(MemoryTraceStore::new()),
            Arc::new(MemoryCostStore::default()),
        );

        // Pending (recoverable)
        lifecycle
            .start_job(SessionId::from("s1"), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        // InProgress (recoverable)
        let j = lifecycle
            .start_job(SessionId::from("s1"), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j.id).await.unwrap();
        // Completed (NOT recoverable)
        let j2 = lifecycle
            .start_job(SessionId::from("s1"), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j2.id).await.unwrap();
        lifecycle
            .complete(
                &j2.id,
                baybo_job::JobOutput::Message {
                    content: vec![ContentBlock::Text("ok".into())],
                    ordinal: None,
                },
            )
            .await
            .unwrap();

        let recoverable = api.find_recoverable_jobs().await.unwrap();
        assert_eq!(recoverable.len(), 2);
        for r in &recoverable {
            assert!(!r.status.is_terminal());
        }
    }

    #[tokio::test]
    async fn lineage_tree_walks_two_levels() {
        let store = Arc::new(MemSessionStore::default());
        let parent = SessionId::from("parent");
        let mid = SessionId::from("mid");
        let leaf = SessionId::from("leaf");
        store
            .children
            .lock()
            .insert(parent.clone(), vec![(mid.clone(), LineageKind::Subagent)]);
        store
            .children
            .lock()
            .insert(mid.clone(), vec![(leaf.clone(), LineageKind::Subagent)]);

        let api = make_query_api(store);
        let tree = api.lineage_tree(&parent).await.unwrap();
        assert_eq!(tree.session_id, parent);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].session_id, mid);
        assert_eq!(tree.children[0].children.len(), 1);
        assert_eq!(tree.children[0].children[0].session_id, leaf);
    }

    #[tokio::test]
    async fn list_active_subagents_filters_terminal() {
        let store = Arc::new(MemSessionStore::default());
        let parent = SessionId::from("parent");
        let active_child = SessionId::from("child-active");
        let done_child = SessionId::from("child-done");
        store.children.lock().insert(
            parent.clone(),
            vec![
                (active_child.clone(), LineageKind::Subagent),
                (done_child.clone(), LineageKind::Subagent),
            ],
        );

        let job_store = Arc::new(MemoryJobStore::new());
        let lifecycle = Arc::new(JobLifecycle::new(job_store.clone()));
        // Active child has an InProgress job
        let j_active = lifecycle
            .start_job(active_child.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j_active.id).await.unwrap();
        // Done child has a Completed job
        let j_done = lifecycle
            .start_job(done_child.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j_done.id).await.unwrap();
        lifecycle
            .complete(
                &j_done.id,
                baybo_job::JobOutput::Message {
                    content: vec![ContentBlock::Text("ok".into())],
                    ordinal: None,
                },
            )
            .await
            .unwrap();

        let api = QueryApi::new(
            store,
            lifecycle,
            Arc::new(MemoryTraceStore::new()),
            Arc::new(MemoryCostStore::default()),
        );
        let live = api.list_active_subagents(&parent).await.unwrap();
        assert_eq!(live, vec![active_child]);
    }

    #[tokio::test]
    async fn replay_returns_jobs_in_chronological_order() {
        let store = Arc::new(MemSessionStore::default());
        let s = make_session("cli-1");
        store.save(&s).await.unwrap();
        let job_store = Arc::new(MemoryJobStore::new());
        let lifecycle = Arc::new(JobLifecycle::new(job_store));

        let _j1 = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        let _j2 = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();

        let api = QueryApi::new(
            store,
            lifecycle,
            Arc::new(MemoryTraceStore::new()),
            Arc::new(MemoryCostStore::default()),
        );
        let replay = api.replay(&s.id, None).await.unwrap();
        assert_eq!(replay.jobs.len(), 2);
        assert!(replay.jobs[0].job.created_at <= replay.jobs[1].job.created_at);
    }

    /// A `Persisted` span with a non-empty `suffix` (compression /
    /// progress-observer framing that is not itself in `session_messages`)
    /// hydrates to the active prefix *followed by* that suffix — the exact
    /// slice the LLM saw, without the prefix being cloned into span storage.
    #[tokio::test]
    async fn replay_appends_persisted_suffix() {
        use baybo_model::{ChatMessage, ContentBlock, SpanId, StepId};
        use baybo_trace::{
            LifecycleState, LlmCallBegin, LlmCallInputs, Span, SpanKind, Step, StepKind, TraceStore,
        };

        fn msg(text: &str) -> ChatMessage {
            ChatMessage::agent_context(vec![ContentBlock::Text(text.into())])
        }

        let session_store = Arc::new(MemSessionStore::default());
        let s = make_session("cli-suffix");
        session_store.save(&s).await.unwrap();
        session_store
            .append_session_message(
                &s.id,
                &ChatMessage::system(vec![ContentBlock::Text("sys".into())]),
            )
            .await
            .unwrap();
        session_store
            .append_session_message(&s.id, &msg("hello"))
            .await
            .unwrap();
        let last = session_store
            .latest_session_ordinal(&s.id)
            .await
            .unwrap()
            .expect("messages appended");
        let active = session_store
            .load_active_session_messages(&s.id)
            .await
            .unwrap();

        let lifecycle = Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new())));
        let j = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j.id).await.unwrap();

        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let step_id = StepId::new();
        let now = Utc::now();
        trace_store
            .save_step(
                &Step {
                    id: step_id,
                    job_id: j.id,
                    kind: StepKind::Compression,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();
        let instruction = msg("SUMMARIZE NOW");
        let span_id = SpanId::new();
        trace_store
            .save_span(
                &Span {
                    id: span_id,
                    step_id,
                    kind: SpanKind::LlmCall {
                        begin: LlmCallBegin {
                            model_id: "claude".into(),
                            provider: "anthropic".into(),
                            provider_config_hash: String::new(),
                            input_messages: LlmCallInputs::Persisted {
                                last_ordinal: last,
                                prefix_len: active.len(),
                                suffix: vec![instruction.clone()],
                            },
                            temperature: None,
                        },
                        result: None,
                    },
                    parallel_group: None,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                    events: vec![],
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();

        let api = QueryApi::new(
            session_store,
            lifecycle,
            trace_store,
            Arc::new(MemoryCostStore::default()),
        );
        let replay = api.replay(&s.id, None).await.unwrap();
        let span = replay
            .jobs
            .iter()
            .flat_map(|j| j.steps.iter())
            .flat_map(|st| st.spans.iter())
            .find(|sp| sp.id == span_id)
            .expect("the compression span survives replay");
        let SpanKind::LlmCall { begin, .. } = &span.kind else {
            unreachable!()
        };
        let LlmCallInputs::Inline(hydrated) = &begin.input_messages else {
            panic!(
                "Persisted must hydrate to Inline; got {:?}",
                begin.input_messages
            );
        };

        let mut expected = active.clone();
        expected.push(instruction);
        assert_eq!(
            hydrated, &expected,
            "hydration must append the inline suffix after the active prefix"
        );
    }

    #[tokio::test]
    async fn replay_hydrates_transcript_backed_tool_output() {
        use baybo_model::{ChatMessage, SpanId, StepId};
        use baybo_trace::{
            LifecycleOutcome, LifecycleState, PersistedToolCallOutput, Span, SpanKind, Step,
            StepKind, ToolCallBegin, ToolCallOutput, ToolCallResult, TraceStore,
        };

        let session_store = Arc::new(MemSessionStore::default());
        let session = make_session("tool-output-ref");
        session_store.save(&session).await.unwrap();
        let span_started_at = Utc::now();
        let wrapped = "<tool_output name=\"Read\">\none durable copy\n</tool_output>";
        session_store
            .append_session_message(
                &session.id,
                &ChatMessage::tool_result("tool-use-1".into(), wrapped.to_string()),
            )
            .await
            .unwrap();

        let lifecycle = Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new())));
        let job = lifecycle
            .start_job(session.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let step = Step {
            id: StepId::new(),
            job_id: job.id,
            kind: StepKind::LlmIteration,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            outcome: LifecycleState::Done(LifecycleOutcome::Ok),
        };
        trace_store
            .save_step(&step.to_row().unwrap())
            .await
            .unwrap();
        let span_id = SpanId::new();
        trace_store
            .save_span(
                &Span {
                    id: span_id,
                    step_id: step.id,
                    kind: SpanKind::ToolCall {
                        begin: ToolCallBegin {
                            tool_name: "Read".into(),
                            tool_artifact_hash: String::new(),
                            triggered_by: None,
                            params: serde_json::json!({}),
                        },
                        result: Some(ToolCallResult {
                            output: ToolCallOutput::persisted(PersistedToolCallOutput::new(
                                "tool-use-1".into(),
                                vec![],
                                vec![],
                            )),
                            success: true,
                            output_truncated_from: None,
                        }),
                    },
                    parallel_group: None,
                    started_at: span_started_at,
                    ended_at: Some(Utc::now()),
                    outcome: LifecycleState::Done(LifecycleOutcome::Ok),
                    events: vec![],
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();

        let api = QueryApi::new(
            session_store,
            lifecycle,
            trace_store,
            Arc::new(MemoryCostStore::default()),
        );
        let replay = api.replay(&session.id, None).await.unwrap();
        let span = replay
            .jobs
            .iter()
            .flat_map(|job| job.steps.iter())
            .flat_map(|step| step.spans.iter())
            .find(|span| span.id == span_id)
            .unwrap();
        let SpanKind::ToolCall {
            result: Some(result),
            ..
        } = &span.kind
        else {
            panic!("expected tool result");
        };
        assert_eq!(
            result.output,
            ToolCallOutput::Inline(serde_json::Value::String(wrapped.to_string()))
        );
    }

    /// Differential test: the write-side "active as of N" snapshot
    /// (`load_active_session_messages_up_to`, captured by the background
    /// summary at call time) and the read-side reconstruction in
    /// `hydrate_persisted_trace_data` must agree — even after the referenced
    /// rows are later superseded by a compaction. `load_*_up_to` is
    /// time-sensitive (only currently-active rows), so the snapshot is
    /// captured BEFORE compaction; replay must reproduce it AFTER. Pins
    /// the equivalence so the two separate filter implementations can't
    /// drift apart silently.
    #[tokio::test]
    async fn replay_matches_write_side_load_up_to_across_compaction() {
        use baybo_model::{ChatMessage, ContentBlock, SpanId, StepId};
        use baybo_trace::{
            LifecycleState, LlmCallBegin, LlmCallInputs, Span, SpanKind, Step, StepKind, TraceStore,
        };
        fn um(t: &str) -> ChatMessage {
            ChatMessage::agent_context(vec![ContentBlock::Text(t.into())])
        }
        fn sm(t: &str) -> ChatMessage {
            ChatMessage::system(vec![ContentBlock::Text(t.into())])
        }

        let session_store = Arc::new(MemSessionStore::default());
        let s = make_session("cli-diff");
        session_store.save(&s).await.unwrap();
        session_store
            .append_session_message(&s.id, &sm("sys"))
            .await
            .unwrap();
        session_store
            .append_session_message(&s.id, &um("u1"))
            .await
            .unwrap();
        let n = session_store
            .latest_session_ordinal(&s.id)
            .await
            .unwrap()
            .expect("messages appended");

        // Write-side snapshot, BEFORE compaction — exactly what the
        // background pass records (prefix + its count for the tripwire).
        let write_side = session_store
            .load_active_session_messages_up_to(&s.id, n)
            .await
            .unwrap();

        // Compaction supersedes ordinals 0,1 (superseded_by = 2) and
        // appends new rows at 2,3 — the referenced rows are now inactive,
        // so only the historical "as of N" filter can still recover them.
        session_store
            .apply_session_compaction(&s.id, &[sm("sys-v2"), um("<summary>S</summary>")])
            .await
            .unwrap();

        let lifecycle = Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new())));
        let j = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j.id).await.unwrap();
        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let step_id = StepId::new();
        let now = Utc::now();
        trace_store
            .save_step(
                &Step {
                    id: step_id,
                    job_id: j.id,
                    kind: StepKind::Compression,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();
        let span_id = SpanId::new();
        trace_store
            .save_span(
                &Span {
                    id: span_id,
                    step_id,
                    kind: SpanKind::LlmCall {
                        begin: LlmCallBegin {
                            model_id: "claude".into(),
                            provider: "anthropic".into(),
                            provider_config_hash: String::new(),
                            input_messages: LlmCallInputs::Persisted {
                                last_ordinal: n,
                                prefix_len: write_side.len(),
                                suffix: vec![],
                            },
                            temperature: None,
                        },
                        result: None,
                    },
                    parallel_group: None,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                    events: vec![],
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();

        let api = QueryApi::new(
            session_store,
            lifecycle,
            trace_store,
            Arc::new(MemoryCostStore::default()),
        );
        let replay = api.replay(&s.id, None).await.unwrap();
        let span = replay
            .jobs
            .iter()
            .flat_map(|j| j.steps.iter())
            .flat_map(|st| st.spans.iter())
            .find(|sp| sp.id == span_id)
            .expect("span survives replay");
        let SpanKind::LlmCall { begin, .. } = &span.kind else {
            unreachable!()
        };
        let LlmCallInputs::Inline(read_side) = &begin.input_messages else {
            panic!(
                "Persisted must hydrate to Inline; got {:?}",
                begin.input_messages
            )
        };
        assert_eq!(
            read_side, &write_side,
            "read-side hydration must reproduce the write-side load_up_to snapshot \
             even after the referenced rows were superseded"
        );
        assert_eq!(
            read_side.len(),
            2,
            "no tripwire marker on a consistent reconstruction"
        );
    }

    /// Negative tripwire test: when the recorded `prefix_len` no longer
    /// matches what the log reconstructs (a `superseded_by` bug, a deleted
    /// row, a read/write filter divergence), hydration must FLAG it with a
    /// visible marker rather than returning a plausible-but-wrong slice.
    /// Simulated by recording a `prefix_len` larger than the rows that
    /// actually reconstruct.
    #[tokio::test]
    async fn replay_flags_prefix_len_tripwire_mismatch() {
        use baybo_model::{ChatMessage, ContentBlock, Role, SpanId, StepId};
        use baybo_trace::{
            LifecycleState, LlmCallBegin, LlmCallInputs, Span, SpanKind, Step, StepKind, TraceStore,
        };
        fn um(t: &str) -> ChatMessage {
            ChatMessage::agent_context(vec![ContentBlock::Text(t.into())])
        }

        let session_store = Arc::new(MemSessionStore::default());
        let s = make_session("cli-tripwire");
        session_store.save(&s).await.unwrap();
        session_store
            .append_session_message(&s.id, &um("a"))
            .await
            .unwrap();
        session_store
            .append_session_message(&s.id, &um("b"))
            .await
            .unwrap();
        let n = session_store
            .latest_session_ordinal(&s.id)
            .await
            .unwrap()
            .expect("messages appended");

        let lifecycle = Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new())));
        let j = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j.id).await.unwrap();
        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let step_id = StepId::new();
        let now = Utc::now();
        trace_store
            .save_step(
                &Step {
                    id: step_id,
                    job_id: j.id,
                    kind: StepKind::Compression,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();
        let span_id = SpanId::new();
        // Record prefix_len = 99 though only 2 rows reconstruct → drift.
        trace_store
            .save_span(
                &Span {
                    id: span_id,
                    step_id,
                    kind: SpanKind::LlmCall {
                        begin: LlmCallBegin {
                            model_id: "claude".into(),
                            provider: "anthropic".into(),
                            provider_config_hash: String::new(),
                            input_messages: LlmCallInputs::Persisted {
                                last_ordinal: n,
                                prefix_len: 99,
                                suffix: vec![],
                            },
                            temperature: None,
                        },
                        result: None,
                    },
                    parallel_group: None,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                    events: vec![],
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();

        let api = QueryApi::new(
            session_store,
            lifecycle,
            trace_store,
            Arc::new(MemoryCostStore::default()),
        );
        let replay = api.replay(&s.id, None).await.unwrap();
        let span = replay
            .jobs
            .iter()
            .flat_map(|j| j.steps.iter())
            .flat_map(|st| st.spans.iter())
            .find(|sp| sp.id == span_id)
            .expect("span survives replay");
        let SpanKind::LlmCall { begin, .. } = &span.kind else {
            unreachable!()
        };
        let LlmCallInputs::Inline(hydrated) = &begin.input_messages else {
            panic!(
                "Persisted must hydrate to Inline; got {:?}",
                begin.input_messages
            )
        };
        let first = hydrated
            .first()
            .expect("a marker must be prepended on mismatch");
        assert_eq!(
            first.role,
            Role::System,
            "the tripwire marker is a system-role message"
        );
        let ContentBlock::Text(text) = first.content.first().expect("marker has a text block")
        else {
            panic!("expected a text marker block");
        };
        assert!(
            text.contains("reconstruction inconsistent"),
            "marker must explain the drift; got: {text}"
        );
        assert_eq!(
            hydrated.len(),
            3,
            "marker + the 2 rows that did reconstruct"
        );
    }

    /// `replay` rehydrates `LlmCallInputs::Persisted { last_ordinal }`
    /// into the active slice as of that ordinal — including across a
    /// compaction. This locks the storage / span / hydration triangle
    /// the PR exists to enable: span storage stays O(1) per call, but
    /// the replay surface still hands consumers the exact transcript
    /// the LLM saw.
    #[tokio::test]
    async fn replay_hydrates_persisted_inputs_across_compaction() {
        use baybo_model::{ChatMessage, ContentBlock, SpanId, StepId};
        use baybo_trace::{
            LifecycleState, LlmCallBegin, LlmCallInputs, Span, SpanKind, Step, StepKind, TraceStore,
        };

        fn user_msg(text: &str) -> ChatMessage {
            ChatMessage::agent_context(vec![ContentBlock::Text(text.into())])
        }
        fn system_msg(text: &str) -> ChatMessage {
            ChatMessage::system(vec![ContentBlock::Text(text.into())])
        }

        let session_store = Arc::new(MemSessionStore::default());
        let s = make_session("cli-hydrate");
        session_store.save(&s).await.unwrap();

        // ---- Pre-compaction transcript: [system, user-1] (ordinals 0, 1).
        session_store
            .append_session_message(&s.id, &system_msg("sys-v1"))
            .await
            .unwrap();
        session_store
            .append_session_message(&s.id, &user_msg("hello"))
            .await
            .unwrap();
        let pre_last = session_store
            .latest_session_ordinal(&s.id)
            .await
            .unwrap()
            .expect("two messages appended");
        assert_eq!(pre_last, 1);
        let pre_active = session_store
            .load_active_session_messages(&s.id)
            .await
            .unwrap();

        // First job + step + span. The span's `last_ordinal` anchors
        // to the transcript at this point in time (ordinals 0..=1).
        let job_store = Arc::new(MemoryJobStore::new());
        let lifecycle = Arc::new(JobLifecycle::new(job_store));
        let j1 = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j1.id).await.unwrap();

        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let step1_id = StepId::new();
        let now = Utc::now();
        trace_store
            .save_step(
                &Step {
                    id: step1_id,
                    job_id: j1.id,
                    kind: StepKind::LlmIteration,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();
        let span1_id = SpanId::new();
        trace_store
            .save_span(
                &Span {
                    id: span1_id,
                    step_id: step1_id,
                    kind: SpanKind::LlmCall {
                        begin: LlmCallBegin {
                            model_id: "claude".into(),
                            provider: "anthropic".into(),
                            provider_config_hash: String::new(),
                            input_messages: LlmCallInputs::Persisted {
                                last_ordinal: pre_last,
                                prefix_len: pre_active.len(),
                                suffix: vec![],
                            },
                            temperature: None,
                        },
                        result: None,
                    },
                    parallel_group: None,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                    events: vec![],
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();

        // ---- Compaction: replace [system, user-1] with [system, summary].
        // Old rows get `superseded_by = 2`; new rows land at ordinals 2, 3.
        let post_compact_active = vec![system_msg("sys-v2"), user_msg("<summary>S</summary>")];
        session_store
            .apply_session_compaction(&s.id, &post_compact_active)
            .await
            .unwrap();

        // Append two more turns post-compaction (ordinals 4, 5).
        session_store
            .append_session_message(&s.id, &user_msg("follow-up"))
            .await
            .unwrap();
        session_store
            .append_session_message(&s.id, &user_msg("again"))
            .await
            .unwrap();

        let post_last = session_store
            .latest_session_ordinal(&s.id)
            .await
            .unwrap()
            .expect("post-compaction messages exist");
        assert_eq!(post_last, 5);

        // Second job + span anchored to the post-compaction transcript.
        let j2 = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j2.id).await.unwrap();

        let step2_id = StepId::new();
        trace_store
            .save_step(
                &Step {
                    id: step2_id,
                    job_id: j2.id,
                    kind: StepKind::LlmIteration,
                    started_at: Utc::now(),
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();
        // Count the active-as-of-`post_last` slice now (before
        // `session_store` moves into the QueryApi) so the span's
        // `prefix_len` tripwire matches what hydration will reconstruct.
        let post_active_count = session_store
            .load_active_session_messages(&s.id)
            .await
            .unwrap()
            .len();
        let span2_id = SpanId::new();
        trace_store
            .save_span(
                &Span {
                    id: span2_id,
                    step_id: step2_id,
                    kind: SpanKind::LlmCall {
                        begin: LlmCallBegin {
                            model_id: "claude".into(),
                            provider: "anthropic".into(),
                            provider_config_hash: String::new(),
                            input_messages: LlmCallInputs::Persisted {
                                last_ordinal: post_last,
                                prefix_len: post_active_count,
                                suffix: vec![],
                            },
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
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();

        // ---- Replay and verify hydration.
        let api = QueryApi::new(
            session_store,
            lifecycle,
            trace_store,
            Arc::new(MemoryCostStore::default()),
        );
        let replay = api.replay(&s.id, None).await.unwrap();

        let all_spans: Vec<&Span> = replay
            .jobs
            .iter()
            .flat_map(|j| j.steps.iter())
            .flat_map(|s| s.spans.iter())
            .collect();
        assert_eq!(all_spans.len(), 2, "expected one LlmCall span per job");

        // Every Persisted reference must have collapsed to Inline:
        // the read surface never leaks the ordinal indirection.
        for span in &all_spans {
            if let SpanKind::LlmCall { begin, .. } = &span.kind {
                assert!(
                    matches!(&begin.input_messages, LlmCallInputs::Inline(_)),
                    "Persisted span should be hydrated to Inline; got {:?}",
                    begin.input_messages
                );
            } else {
                panic!("expected LlmCall span");
            }
        }

        let span1 = all_spans.iter().find(|s| s.id == span1_id).unwrap();
        let span2 = all_spans.iter().find(|s| s.id == span2_id).unwrap();
        let SpanKind::LlmCall { begin: b1, .. } = &span1.kind else {
            unreachable!()
        };
        let SpanKind::LlmCall { begin: b2, .. } = &span2.kind else {
            unreachable!()
        };
        let LlmCallInputs::Inline(hydrated_pre) = &b1.input_messages else {
            unreachable!()
        };
        let LlmCallInputs::Inline(hydrated_post) = &b2.input_messages else {
            unreachable!()
        };

        // Pre-compaction span: the slice the LLM saw at ordinal 1 is
        // exactly what `load_active_session_messages` returned at the
        // time. The standard "active as of X" filter must pull
        // through superseded rows whose `superseded_by` is *later*
        // than the anchor ordinal — which is the whole point of the
        // ordinal indirection.
        assert_eq!(hydrated_pre, &pre_active);

        // Post-compaction span: same shape, anchored to the present.
        let post_active = api
            .sessions
            .load_active_session_messages(&s.id)
            .await
            .unwrap();
        assert_eq!(hydrated_post, &post_active);
        assert_eq!(hydrated_post.len(), 4); // [sys-v2, summary, follow-up, again]
    }

    /// `load_trace_overview` returns the full `session_messages`
    /// log (including superseded rows so the client can still slice
    /// pre-compaction spans) and the job summaries oldest-first.
    /// No step/span data — that's the cheap-on-purpose contract.
    #[tokio::test]
    async fn load_trace_overview_returns_message_log_and_summaries() {
        use baybo_model::{ChatMessage, ContentBlock};

        fn user_msg(text: &str) -> ChatMessage {
            ChatMessage::agent_context(vec![ContentBlock::Text(text.into())])
        }

        let session_store = Arc::new(MemSessionStore::default());
        let s = make_session("overview-1");
        session_store.save(&s).await.unwrap();
        session_store
            .append_session_message(&s.id, &user_msg("m0"))
            .await
            .unwrap();
        session_store
            .append_session_message(&s.id, &user_msg("m1"))
            .await
            .unwrap();
        // Compact so `m0`/`m1` get a `superseded_by` marker.
        session_store
            .apply_session_compaction(&s.id, &[user_msg("summary")])
            .await
            .unwrap();

        let job_store = Arc::new(MemoryJobStore::new());
        let lifecycle = Arc::new(JobLifecycle::new(job_store));
        let _j1 = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        let _j2 = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();

        let api = QueryApi::new(
            session_store,
            lifecycle,
            Arc::new(MemoryTraceStore::new()),
            Arc::new(MemoryCostStore::default()),
        );
        let overview = api.load_trace_overview(&s.id, None).await.unwrap();

        assert_eq!(overview.session_id, s.id);
        // Pre-compaction rows are preserved, with `superseded_by`
        // set so the client can replicate the "active as of X" filter.
        assert_eq!(overview.session_messages.len(), 3);
        assert_eq!(overview.session_messages[0].ordinal, 0);
        assert_eq!(overview.session_messages[0].superseded_by, Some(2));
        assert_eq!(overview.session_messages[1].ordinal, 1);
        assert_eq!(overview.session_messages[1].superseded_by, Some(2));
        assert_eq!(overview.session_messages[2].ordinal, 2);
        assert_eq!(overview.session_messages[2].superseded_by, None);

        assert_eq!(overview.jobs.len(), 2);
        assert!(overview.jobs[0].summary.created_at <= overview.jobs[1].summary.created_at);
        assert_eq!(overview.supersede_watermark, Some(2));

        // Incremental poll: only rows above the client's ordinal, same
        // watermark so the client can validate its cached prefix.
        let delta = api.load_trace_overview(&s.id, Some(1)).await.unwrap();
        assert_eq!(delta.session_messages.len(), 1);
        assert_eq!(delta.session_messages[0].ordinal, 2);
        assert_eq!(delta.supersede_watermark, Some(2));
        assert_eq!(delta.jobs.len(), 2, "job summaries always ship in full");
    }

    #[tokio::test]
    async fn list_session_summaries_paginates_before_aggregating() {
        use baybo_trace::{
            LifecycleState, LlmCallBegin, LlmCallInputs, Span, SpanKind, Step, StepKind,
        };

        let session_store = Arc::new(MemSessionStore::default());
        let job_store = Arc::new(MemoryJobStore::new());
        let trace_store = Arc::new(MemoryTraceStore::new());
        let base = Utc::now();

        // Three sessions with one job each (newest-active first), plus a
        // zero-job session that must stay invisible even though it is
        // the newest of all.
        let mut with_jobs = Vec::new();
        for (i, name) in ["sum-new", "sum-mid", "sum-old"].iter().enumerate() {
            let mut s = make_session(name);
            s.last_active = base - chrono::Duration::hours(i as i64);
            session_store.save(&s).await.unwrap();
            let mut j = Job::new(s.id.clone(), TriggerKind::User, user_input(), None);
            j.created_at = base;
            job_store.create(&j.to_row().unwrap()).await.unwrap();
            with_jobs.push((s, j));
        }
        let mut empty = make_session("sum-empty");
        empty.last_active = base + chrono::Duration::hours(1);
        session_store.save(&empty).await.unwrap();

        // "sum-mid" carries one step with two spans.
        let (mid_session, mid_job) = &with_jobs[1];
        let step = Step {
            id: StepId::new(),
            job_id: mid_job.id,
            kind: StepKind::LlmIteration,
            started_at: base,
            ended_at: None,
            outcome: LifecycleState::Pending,
        };
        trace_store
            .save_step(&step.to_row().unwrap())
            .await
            .unwrap();
        for _ in 0..2 {
            let span = Span {
                id: baybo_model::SpanId::new(),
                step_id: step.id,
                kind: SpanKind::LlmCall {
                    begin: LlmCallBegin {
                        model_id: "m".into(),
                        provider: "p".into(),
                        provider_config_hash: "h".into(),
                        input_messages: LlmCallInputs::empty(),
                        temperature: None,
                    },
                    result: None,
                },
                parallel_group: None,
                started_at: base,
                ended_at: None,
                outcome: LifecycleState::Pending,
                events: vec![],
            };
            trace_store
                .save_span(&span.to_row().unwrap())
                .await
                .unwrap();
        }

        let api = QueryApi::new(
            session_store,
            Arc::new(JobLifecycle::new(job_store)),
            trace_store,
            Arc::new(MemoryCostStore::default()),
        );
        let listing = api
            .list_session_summaries(
                SessionSummaryFilter::default(),
                SessionSummaryPage {
                    offset: 1,
                    limit: 1,
                },
            )
            .await
            .unwrap();

        assert_eq!(listing.total, 3, "zero-job session must not count");
        assert_eq!(listing.items.len(), 1);
        let item = &listing.items[0];
        assert_eq!(item.session_id, mid_session.id);
        assert_eq!(item.job_count, 1);
        assert_eq!(item.span_count, 2);
        assert!(matches!(item.latest_job_status, Some(JobStatus::Pending)));
    }

    #[tokio::test]
    async fn list_session_summaries_status_filter_matches_latest_job_only() {
        let session_store = Arc::new(MemSessionStore::default());
        let job_store = Arc::new(MemoryJobStore::new());
        let s = make_session("sum-status");
        session_store.save(&s).await.unwrap();
        let base = Utc::now();

        let mut done = Job::new(s.id.clone(), TriggerKind::User, user_input(), None);
        done.created_at = base;
        let _ = done.start().unwrap();
        let _ = done
            .complete(baybo_job::JobOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        job_store.create(&done.to_row().unwrap()).await.unwrap();

        let mut pending = Job::new(s.id.clone(), TriggerKind::User, user_input(), None);
        pending.created_at = base + chrono::Duration::seconds(1);
        job_store.create(&pending.to_row().unwrap()).await.unwrap();

        let api = QueryApi::new(
            session_store,
            Arc::new(JobLifecycle::new(job_store)),
            Arc::new(MemoryTraceStore::new()),
            Arc::new(MemoryCostStore::default()),
        );
        let page = SessionSummaryPage {
            offset: 0,
            limit: 0,
        };

        let completed = api
            .list_session_summaries(
                SessionSummaryFilter {
                    status_kind: Some(JobStatusKind::Completed),
                    ..Default::default()
                },
                page,
            )
            .await
            .unwrap();
        assert_eq!(
            completed.total, 0,
            "an older completed job must not match — the latest job is pending"
        );

        let pending_hits = api
            .list_session_summaries(
                SessionSummaryFilter {
                    status_kind: Some(JobStatusKind::Pending),
                    ..Default::default()
                },
                page,
            )
            .await
            .unwrap();
        assert_eq!(pending_hits.total, 1);
        assert_eq!(pending_hits.items[0].job_count, 2);
        assert!(matches!(
            pending_hits.items[0].latest_job_status,
            Some(JobStatus::Pending)
        ));
    }

    /// `load_job_trace` returns the per-job step/span tree with
    /// `LlmCallInputs::Persisted` references **unchanged** — the
    /// client is expected to slice them against the message log
    /// served by `load_trace_overview`.
    #[tokio::test]
    async fn load_job_trace_preserves_persisted_inputs() {
        use baybo_model::{ChatMessage, ContentBlock, SpanId, StepId};
        use baybo_trace::{
            LifecycleState, LlmCallBegin, LlmCallInputs, Span, SpanKind, Step, StepKind, TraceStore,
        };

        fn user_msg(text: &str) -> ChatMessage {
            ChatMessage::agent_context(vec![ContentBlock::Text(text.into())])
        }

        let session_store = Arc::new(MemSessionStore::default());
        let s = make_session("job-trace-1");
        session_store.save(&s).await.unwrap();
        session_store
            .append_session_message(&s.id, &user_msg("hi"))
            .await
            .unwrap();
        let last = session_store
            .latest_session_ordinal(&s.id)
            .await
            .unwrap()
            .unwrap();

        let job_store = Arc::new(MemoryJobStore::new());
        let lifecycle = Arc::new(JobLifecycle::new(job_store));
        let j = lifecycle
            .start_job(s.id.clone(), TriggerKind::User, user_input(), None)
            .await
            .unwrap();
        lifecycle.start(&j.id).await.unwrap();

        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let step_id = StepId::new();
        let now = Utc::now();
        trace_store
            .save_step(
                &Step {
                    id: step_id,
                    job_id: j.id,
                    kind: StepKind::LlmIteration,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();
        let span_id = SpanId::new();
        trace_store
            .save_span(
                &Span {
                    id: span_id,
                    step_id,
                    kind: SpanKind::LlmCall {
                        begin: LlmCallBegin {
                            model_id: "claude".into(),
                            provider: "anthropic".into(),
                            provider_config_hash: String::new(),
                            input_messages: LlmCallInputs::Persisted {
                                last_ordinal: last,
                                prefix_len: 1,
                                suffix: vec![],
                            },
                            temperature: None,
                        },
                        result: None,
                    },
                    parallel_group: None,
                    started_at: now,
                    ended_at: None,
                    outcome: LifecycleState::Pending,
                    events: vec![],
                }
                .to_row()
                .unwrap(),
            )
            .await
            .unwrap();

        let api = QueryApi::new(
            session_store,
            lifecycle,
            trace_store,
            Arc::new(MemoryCostStore::default()),
        );
        let job_trace = api.load_job_trace(&j.id).await.unwrap();

        assert_eq!(job_trace.job.id, j.id);
        assert_eq!(job_trace.steps.len(), 1);
        assert_eq!(job_trace.steps[0].spans.len(), 1);
        let SpanKind::LlmCall { begin, .. } = &job_trace.steps[0].spans[0].kind else {
            panic!("expected LlmCall span");
        };
        assert!(
            matches!(begin.input_messages, LlmCallInputs::Persisted { .. }),
            "load_job_trace must preserve Persisted refs; got {:?}",
            begin.input_messages
        );
    }
}
