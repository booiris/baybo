//! Query API v1 — the read surface over Session / Job / Step / Span,
//! defined as the union of the read paths in `docs/modules/{session,job,trace}.md`.
//!
//! 9 endpoints:
//!
//! 1. `load_session` — resolves lineage
//! 2. `list_jobs` — fork-prefix UNION + `is_inherited` flag
//! 3. `load_job` — Job + step list
//! 4. `load_step` — Step + spans + span events
//! 5. `find_recoverable_jobs` — recovery scan
//! 6. `list_active_subagents` — live Subagent-lineage children
//! 7. `lineage_tree` — ancestry + immediate descendants
//! 8. `cost_summary` — User / Session / Job / TimeRange
//! 9. `replay` — chronological Job → Step → Span tree (also the
//!    backend for fork's view-layer UNION)
//!
//! Errors collapse into a single `QueryError` so callers don't need to
//! match four different store error types.

use std::collections::HashMap;
use std::sync::Arc;

use aura_job::{Job, JobError, JobKind, JobStatus, JobStatusKind};
use aura_model::{JobId, Lineage, LineageKind, MicroUsd, Session, SessionId, StepId};
use aura_storage::{
    CostError, CostStore, CostSummary, SessionStore, StorageError, TimeRange, TraceStore,
};
use aura_trace::{Span, SpanEvent, Step, TraceError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::job::JobLifecycle;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error("session error: {0}")]
    Session(#[from] StorageError),
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
    pub kind: Option<JobKind>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// Lightweight row returned by `list_jobs`. `is_inherited = true`
/// means the job lives in another session and is being surfaced via
/// the fork view-layer UNION; `inherited_from_session_id` then
/// names the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: JobId,
    pub session_id: SessionId,
    pub kind: JobKind,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub is_inherited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_from_session_id: Option<SessionId>,
}

impl JobSummary {
    fn from_owned(j: &Job) -> Self {
        Self {
            id: j.id,
            session_id: j.session_id.clone(),
            kind: j.kind,
            status: j.status.clone(),
            created_at: j.created_at,
            started_at: j.started_at,
            ended_at: j.ended_at,
            is_inherited: false,
            inherited_from_session_id: None,
        }
    }

    fn from_inherited(j: &Job, source: SessionId) -> Self {
        Self {
            id: j.id,
            session_id: j.session_id.clone(),
            kind: j.kind,
            status: j.status.clone(),
            created_at: j.created_at,
            started_at: j.started_at,
            ended_at: j.ended_at,
            is_inherited: true,
            inherited_from_session_id: Some(source),
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

/// Filter for [`QueryApi::list_session_summaries`]. All fields are
/// AND-combined; `None` means no constraint. `status_kind` matches
/// against the *latest* job's `JobStatusKind` (not any historical job).
#[derive(Debug, Clone, Default)]
pub struct SessionSummaryFilter {
    pub status_kind: Option<JobStatusKind>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub session_id_prefix: Option<String>,
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
pub struct ReplayJob {
    pub job: Job,
    pub steps: Vec<ReplayStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayStep {
    pub step: Step,
    pub spans: Vec<Span>,
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
    /// (CLI `aura trace list/show/export`, gateway `/v1/traces/{id}`)
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
        Ok(self.sessions.get(id).await?)
    }

    // ── 2. list_jobs (with fork-prefix UNION) ──────────────────

    pub async fn list_jobs(
        &self,
        session_id: &SessionId,
        filter: JobFilter,
    ) -> Result<Vec<JobSummary>> {
        // Always include this session's own jobs first.
        let mut summaries: Vec<JobSummary> = self
            .jobs
            .list_by_session(session_id, filter.status_kind)
            .await?
            .into_iter()
            .filter(|j| filter_matches(j, &filter))
            .map(|j| JobSummary::from_owned(&j))
            .collect();

        // If this is a UserFork session, prepend the source's prefix.
        if let Some(session) = self.sessions.get(session_id).await?
            && let Some(Lineage {
                parent_session_id,
                kind: LineageKind::UserFork { fork_at_job_id, .. },
                ..
            }) = session.lineage
        {
            let source_jobs: Vec<Job> = self
                .jobs
                .list_by_session(&parent_session_id, filter.status_kind)
                .await?
                .into_iter()
                .filter(|j| filter_matches(j, &filter))
                .collect();
            // Tie-break by JobId after `created_at` so two jobs minted in
            // the same microsecond on the source session don't both pass
            // the cutoff (or both get dropped). `created_at` resolution
            // is µs; ULID JobIds give a deterministic secondary order.
            let cutoff = source_jobs
                .iter()
                .find(|j| j.id == fork_at_job_id)
                .map(|j| (j.created_at, j.id));
            let prefix: Vec<JobSummary> = source_jobs
                .into_iter()
                .filter(|j| match cutoff {
                    Some((c_at, c_id)) => (j.created_at, j.id) <= (c_at, c_id),
                    None => true,
                })
                .map(|j| JobSummary::from_inherited(&j, parent_session_id.clone()))
                .collect();
            let mut combined = prefix;
            combined.append(&mut summaries);
            summaries = combined;
        }
        // Newest-first (already sorted by JobLifecycle::list, but the
        // UNION above interleaved two streams).
        summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(summaries)
    }

    // ── 3. load_job ────────────────────────────────────────────

    pub async fn load_job(&self, id: &JobId) -> Result<JobDetail> {
        let job = self
            .jobs
            .get(id)
            .await?
            .ok_or_else(|| QueryError::NotFound(format!("job {id}")))?;
        let steps = self.trace.list_steps_by_job(id).await?;
        Ok(JobDetail { job, steps })
    }

    // ── 4. load_step ───────────────────────────────────────────

    pub async fn load_step(&self, id: &StepId) -> Result<StepDetail> {
        let step = self
            .trace
            .load_step(id)
            .await?
            .ok_or_else(|| QueryError::NotFound(format!("step {id}")))?;
        let mut spans = self.trace.list_spans_by_step(id).await?;
        // Span events are stored separately; fold them in so callers
        // get a fully self-contained `StepDetail`.
        for span in &mut spans {
            if span.events.is_empty() {
                let evs: Vec<SpanEvent> = self.trace.list_span_events(&span.id).await?;
                span.events = evs;
            }
        }
        Ok(StepDetail { step, spans })
    }

    // ── 5. find_recoverable_jobs ───────────────────────────────

    pub async fn find_recoverable_jobs(&self) -> Result<Vec<Job>> {
        Ok(self.jobs.list_recoverable().await?)
    }

    // ── 6. list_active_subagents ───────────────────────────────

    pub async fn list_active_subagents(&self, session_id: &SessionId) -> Result<Vec<SessionId>> {
        let children = self.sessions.list_lineage_children(session_id).await?;
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
            let kids = self.sessions.list_lineage_children(&parent_id).await?;
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
            CostScope::TimeRange(range) => Ok(costs.query_global(range).await?),
            CostScope::User { user_id, range } => {
                let records = costs.query_user(&user_id, range).await?;
                let mut summary = CostSummary::default();
                for r in records {
                    summary.total_cost_usd += r.cost_usd;
                    summary.total_input_tokens += r.input_tokens;
                    summary.total_output_tokens += r.output_tokens;
                    summary.total_cached_input_tokens += r.cached_input_tokens;
                    summary.total_cache_creation_input_tokens += r.cache_creation_input_tokens;
                    summary.record_count += 1;
                }
                Ok(summary)
            }
            CostScope::Session(sid) => Ok(costs.query_session(&sid).await?),
            CostScope::Job(jid) => Ok(costs.query_job(&jid).await?),
        }
    }

    // ── 10. list_session_summaries ─────────────────────────────

    /// List session summaries for the trace browser. Filters apply
    /// against `last_active` (time range), session id prefix, and the
    /// **latest** job's status kind. Sessions with zero jobs are
    /// dropped (a session with no trace is invisible to the browser).
    ///
    /// Cardinality: this iterates every live session and loads its
    /// jobs/steps/spans to compute aggregates. Acceptable for the
    /// admin surface (low request rate, modest session counts) and
    /// avoids new storage methods. If session counts grow, the
    /// natural follow-up is a denormalised per-session counter table.
    pub async fn list_session_summaries(
        &self,
        filter: SessionSummaryFilter,
        page: SessionSummaryPage,
    ) -> Result<SessionSummaryListing> {
        let mut sessions = self.sessions.list_all().await?;

        // Cheap filters first so we don't pay per-session aggregate
        // costs for rows about to be dropped.
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

        // Now compute aggregates + apply latest-status filter +
        // drop sessions with no jobs.
        let mut summaries: Vec<SessionSummary> = Vec::new();
        for session in &sessions {
            let mut jobs = self.jobs.list_by_session(&session.id, None).await?;
            if jobs.is_empty() {
                continue;
            }
            // `list_by_session` returns newest-first, so the head is
            // the latest job by `created_at`.
            jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            let latest = jobs.first();
            if let Some(want) = filter.status_kind
                && latest.is_none_or(|j| j.status.kind() != want)
            {
                continue;
            }

            let mut span_count = 0usize;
            for job in &jobs {
                let steps = self.trace.list_steps_by_job(&job.id).await?;
                for step in &steps {
                    let spans = self.trace.list_spans_by_step(&step.id).await?;
                    span_count += spans.len();
                }
            }

            let (input_tokens, output_tokens, cached_input_tokens, cache_creation_input_tokens) =
                match self.costs.as_ref() {
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

            summaries.push(SessionSummary {
                session_id: session.id.clone(),
                created_at: session.created_at,
                last_active: session.last_active,
                latest_job_status: latest.map(|j| j.status.clone()),
                job_count: jobs.len(),
                span_count,
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
            });
        }

        // `SessionStore::list_all` already orders by `last_active`
        // DESC, but the per-session work above preserves that order.
        let total = summaries.len();
        let start = page.offset.min(total);
        let end = if page.limit == 0 {
            total
        } else {
            start.saturating_add(page.limit).min(total)
        };
        let items = summaries[start..end].to_vec();
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
            cursor = cursor.succ_opt().unwrap_or(cursor);
        }

        let mut total_input = 0usize;
        let mut total_output = 0usize;
        let mut total_cached = 0usize;
        let mut total_cache_create = 0usize;
        let mut total_cost = MicroUsd::ZERO;
        let mut total_records = 0usize;
        let mut by_model: HashMap<String, AnalyticsModelBucket> = HashMap::new();

        let records = costs.query_records_in_range(range.clone()).await?;
        for r in &records {
            total_input += r.input_tokens;
            total_output += r.output_tokens;
            total_cached += r.cached_input_tokens;
            total_cache_create += r.cache_creation_input_tokens;
            total_cost += r.cost_usd;
            total_records += 1;

            let day_key = r.timestamp.date_naive().format("%Y-%m-%d").to_string();
            if let Some(&i) = day_index.get(&day_key) {
                let bucket = &mut daily[i];
                bucket.input_tokens += r.input_tokens;
                bucket.output_tokens += r.output_tokens;
                bucket.cached_input_tokens += r.cached_input_tokens;
                bucket.cache_creation_input_tokens += r.cache_creation_input_tokens;
                bucket.cost_usd += r.cost_usd;
            }

            let entry = by_model
                .entry(r.model.clone())
                .or_insert_with(|| AnalyticsModelBucket {
                    model: r.model.clone(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cost_usd: MicroUsd::ZERO,
                    call_count: 0,
                });
            entry.input_tokens += r.input_tokens;
            entry.output_tokens += r.output_tokens;
            entry.cached_input_tokens += r.cached_input_tokens;
            entry.cache_creation_input_tokens += r.cache_creation_input_tokens;
            entry.cost_usd += r.cost_usd;
            entry.call_count += 1;
        }

        // sessions_created per day. Single SessionStore::list_all call;
        // sessions outside the range are skipped.
        for s in self.sessions.list_all().await? {
            if s.created_at < range.from || s.created_at >= range.to {
                continue;
            }
            let day_key = s.created_at.date_naive().format("%Y-%m-%d").to_string();
            if let Some(&i) = day_index.get(&day_key) {
                daily[i].sessions_created += 1;
            }
        }

        let mut model_buckets: Vec<AnalyticsModelBucket> = by_model.into_values().collect();
        model_buckets.sort_by(|a, b| {
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
        })
    }

    // ── 9. replay ──────────────────────────────────────────────

    pub async fn replay(
        &self,
        session_id: &SessionId,
        until_step_id: Option<StepId>,
    ) -> Result<ReplayedConversation> {
        // Use `list_jobs` so fork-prefix UNION is honoured.
        let summaries = self.list_jobs(session_id, JobFilter::default()).await?;
        // Re-sort oldest-first for replay.
        let mut summaries = summaries;
        summaries.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        let mut jobs = Vec::with_capacity(summaries.len());
        // Truncation lookup is only needed when `until_step_id` is set.
        // Skipping it on the common (CLI / gateway) path saves one
        // `list_steps_by_job` round-trip per job.
        let truncate_after_job: Option<JobId> = match until_step_id.as_ref() {
            None => None,
            Some(target) => {
                let mut found = None;
                for s in &summaries {
                    let steps = self.trace.list_steps_by_job(&s.id).await?;
                    if steps.iter().any(|st| &st.id == target) {
                        found = Some(s.id);
                        break;
                    }
                }
                found
            }
        };

        for s in &summaries {
            let job = match self.jobs.get(&s.id).await? {
                Some(j) => j,
                None => continue, // deleted between calls; skip
            };
            let mut steps = self.trace.list_steps_by_job(&job.id).await?;
            steps.sort_by_key(|s| s.started_at);
            let mut step_blocks = Vec::with_capacity(steps.len());
            for step in steps {
                let mut spans = self.trace.list_spans_by_step(&step.id).await?;
                for span in &mut spans {
                    if span.events.is_empty() {
                        span.events = self.trace.list_span_events(&span.id).await?;
                    }
                }
                let stop_after = until_step_id == Some(step.id);
                step_blocks.push(ReplayStep { step, spans });
                if stop_after {
                    break;
                }
            }
            jobs.push(ReplayJob {
                job,
                steps: step_blocks,
            });
            if Some(s.id) == truncate_after_job {
                break;
            }
        }

        // Hydrate any `LlmCallInputs::Persisted` spans into the flat
        // `Inline` shape the trace API serializes. The session_messages
        // log is read once per session and reused for every Persisted
        // span — turn `O(N²)` storage into `O(N)` storage plus one
        // join on read.
        self.hydrate_persisted_inputs(session_id, &mut jobs).await?;

        Ok(ReplayedConversation {
            session_id: session_id.clone(),
            jobs,
        })
    }

    /// Walk every span in `jobs`, and for each `LlmCall` whose
    /// `input_messages` is `Persisted { last_ordinal, .. }` replace
    /// it with the corresponding `Inline` slice reconstructed from
    /// `session_messages`. Skips the work entirely if no span needs
    /// hydration; reads the session log once when at least one does.
    async fn hydrate_persisted_inputs(
        &self,
        session_id: &SessionId,
        jobs: &mut [ReplayJob],
    ) -> Result<()> {
        use aura_trace::{LlmCallInputs, SpanKind};

        let any_persisted = jobs.iter().any(|j| {
            j.steps.iter().any(|s| {
                s.spans.iter().any(|sp| {
                    matches!(
                        &sp.kind,
                        SpanKind::LlmCall { begin, .. } if begin.input_messages.is_persisted()
                    )
                })
            })
        });
        if !any_persisted {
            return Ok(());
        }

        let log = self
            .sessions
            .load_session_messages_with_supersede(session_id)
            .await?;

        for job in jobs.iter_mut() {
            for step in job.steps.iter_mut() {
                for span in step.spans.iter_mut() {
                    let span_started_at = span.started_at;
                    if let SpanKind::LlmCall { begin, .. } = &mut span.kind
                        && let LlmCallInputs::Persisted { last_ordinal, .. } = &begin.input_messages
                    {
                        let last = *last_ordinal;
                        let candidates: Vec<&aura_storage::StoredMessage> = log
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
                        let hydrated: Vec<aura_model::ChatMessage> = if mismatch {
                            tracing::warn!(
                                session_id = %session_id,
                                span_id = %span.id,
                                last_ordinal = last,
                                "session_messages epoch mismatch — span predates current log; \
                                 returning empty input"
                            );
                            Vec::new()
                        } else {
                            candidates.iter().map(|m| m.message.clone()).collect()
                        };
                        begin.input_messages = LlmCallInputs::Inline(hydrated);
                    }
                }
            }
        }
        Ok(())
    }
}

fn filter_matches(j: &Job, f: &JobFilter) -> bool {
    if let Some(k) = f.kind
        && j.kind != k
    {
        return false;
    }
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
    use aura_job::JobInput;
    use aura_model::{ChannelType, ContentBlock, TriggerKind, TriggerSource};
    use aura_storage::SessionStore;
    use aura_storage::test_support::{MemoryCostStore, MemoryJobStore, MemoryTraceStore};
    use std::sync::Arc;

    fn user_input() -> JobInput {
        JobInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    /// Minimal in-memory `SessionStore` for query-API tests. The
    /// `session_messages` log is a real append-only vector so the
    /// hydration test can drive the same supersede semantics the
    /// libsql backend implements.
    #[derive(Default)]
    struct MemSessionStore {
        sessions: parking_lot::Mutex<HashMap<SessionId, Session>>,
        children: parking_lot::Mutex<HashMap<SessionId, Vec<(SessionId, LineageKind)>>>,
        messages: parking_lot::Mutex<HashMap<SessionId, Vec<aura_storage::StoredMessage>>>,
    }

    #[async_trait::async_trait]
    impl SessionStore for MemSessionStore {
        async fn get(&self, id: &SessionId) -> std::result::Result<Option<Session>, StorageError> {
            Ok(self.sessions.lock().get(id).cloned())
        }
        async fn save(&self, session: &Session) -> std::result::Result<(), StorageError> {
            self.sessions
                .lock()
                .insert(session.id.clone(), session.clone());
            Ok(())
        }
        async fn set_hidden(
            &self,
            id: &SessionId,
            hidden: bool,
        ) -> std::result::Result<bool, StorageError> {
            let mut data = self.sessions.lock();
            match data.get_mut(id) {
                Some(s) => {
                    s.hidden = hidden;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        async fn delete(&self, _id: &SessionId) -> std::result::Result<bool, StorageError> {
            Ok(true)
        }
        async fn list_expired(
            &self,
            _before: DateTime<Utc>,
        ) -> std::result::Result<Vec<SessionId>, StorageError> {
            Ok(Vec::new())
        }
        async fn list_all(&self) -> std::result::Result<Vec<Session>, StorageError> {
            Ok(self.sessions.lock().values().cloned().collect())
        }
        async fn list_live_forks(
            &self,
            _src: &SessionId,
        ) -> std::result::Result<Vec<SessionId>, StorageError> {
            Ok(Vec::new())
        }
        async fn list_lineage_children(
            &self,
            parent: &SessionId,
        ) -> std::result::Result<Vec<(SessionId, LineageKind)>, StorageError> {
            Ok(self
                .children
                .lock()
                .get(parent)
                .cloned()
                .unwrap_or_default())
        }
        async fn list_active_maintenance_for_parent(
            &self,
            _parent: &SessionId,
        ) -> std::result::Result<Vec<SessionId>, StorageError> {
            Ok(Vec::new())
        }
        async fn list_all_maintenance_sessions(
            &self,
        ) -> std::result::Result<Vec<SessionId>, StorageError> {
            Ok(Vec::new())
        }
        async fn list_unfinished_maintenance_sessions(
            &self,
        ) -> std::result::Result<Vec<SessionId>, StorageError> {
            Ok(Vec::new())
        }
        async fn append_session_message(
            &self,
            id: &SessionId,
            message: &aura_model::ChatMessage,
        ) -> std::result::Result<(), StorageError> {
            let mut guard = self.messages.lock();
            let log = guard.entry(id.clone()).or_default();
            let ordinal = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
            log.push(aura_storage::StoredMessage {
                ordinal,
                superseded_by: None,
                created_at: chrono::Utc::now(),
                message: message.clone(),
            });
            Ok(())
        }
        async fn apply_session_compaction(
            &self,
            id: &SessionId,
            new_active: &[aura_model::ChatMessage],
        ) -> std::result::Result<(), StorageError> {
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
                log.push(aura_storage::StoredMessage {
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
        ) -> std::result::Result<Vec<aura_model::ChatMessage>, StorageError> {
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
        ) -> std::result::Result<Option<i64>, StorageError> {
            Ok(self
                .messages
                .lock()
                .get(id)
                .and_then(|log| log.iter().map(|m| m.ordinal).max()))
        }
        async fn load_session_messages_with_supersede(
            &self,
            id: &SessionId,
        ) -> std::result::Result<Vec<aura_storage::StoredMessage>, StorageError> {
            Ok(self.messages.lock().get(id).cloned().unwrap_or_default())
        }
        async fn active_index_of_ordinal(
            &self,
            id: &SessionId,
            ordinal: i64,
        ) -> std::result::Result<Option<usize>, StorageError> {
            Ok(self.messages.lock().get(id).and_then(|log| {
                log.iter()
                    .filter(|m| m.superseded_by.is_none())
                    .position(|m| m.ordinal == ordinal)
            }))
        }
        async fn count_active_messages(
            &self,
            id: &SessionId,
        ) -> std::result::Result<usize, StorageError> {
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
        ) -> std::result::Result<Vec<aura_model::ChatMessage>, StorageError> {
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
        ) -> std::result::Result<Vec<(i64, aura_model::ChatMessage)>, StorageError> {
            if limit == 0 {
                return Ok(Vec::new());
            }
            Ok(self
                .messages
                .lock()
                .get(id)
                .map(|log| {
                    let active: Vec<&aura_storage::StoredMessage> = log
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
                        .map(|m| (m.ordinal, m.message.clone()))
                        .collect()
                })
                .unwrap_or_default())
        }
        async fn load_active_session_messages_since(
            &self,
            id: &SessionId,
            after_ordinal: i64,
            limit: usize,
        ) -> std::result::Result<Vec<(i64, aura_model::ChatMessage)>, StorageError> {
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
                        .map(|m| (m.ordinal, m.message.clone()))
                        .collect()
                })
                .unwrap_or_default())
        }
    }

    fn make_session(id: &str) -> Session {
        let id = SessionId::from(id);
        Session {
            id: id.clone(),
            user: aura_model::User {
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
            bound_soul_version: "soul-test".into(),
            hidden: false,
        }
    }

    fn make_query_api(sessions: Arc<dyn SessionStore>) -> QueryApi {
        let job_store = Arc::new(MemoryJobStore::new());
        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let cost_store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let lifecycle = Arc::new(JobLifecycle::new(job_store));
        QueryApi::new(sessions, lifecycle, trace_store, cost_store)
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
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        // InProgress (recoverable)
        let j = lifecycle
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lifecycle.start(&j.id).await.unwrap();
        // Completed (NOT recoverable)
        let j2 = lifecycle
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lifecycle.start(&j2.id).await.unwrap();
        lifecycle
            .complete(
                &j2.id,
                aura_job::JobOutput::Message {
                    content: vec![ContentBlock::Text("ok".into())],
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
            .start_job(
                active_child.clone(),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lifecycle.start(&j_active.id).await.unwrap();
        // Done child has a Completed job
        let j_done = lifecycle
            .start_job(
                done_child.clone(),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lifecycle.start(&j_done.id).await.unwrap();
        lifecycle
            .complete(
                &j_done.id,
                aura_job::JobOutput::Message {
                    content: vec![ContentBlock::Text("ok".into())],
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
            .start_job(
                s.id.clone(),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        let _j2 = lifecycle
            .start_job(
                s.id.clone(),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
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

    /// `replay` rehydrates `LlmCallInputs::Persisted { last_ordinal }`
    /// into the active slice as of that ordinal — including across a
    /// compaction. This locks the storage / span / hydration triangle
    /// the PR exists to enable: span storage stays O(1) per call, but
    /// the replay surface still hands consumers the exact transcript
    /// the LLM saw.
    #[tokio::test]
    async fn replay_hydrates_persisted_inputs_across_compaction() {
        use aura_model::{ChatMessage, ContentBlock, Role, SpanId, StepId};
        use aura_storage::TraceStore;
        use aura_trace::{
            LifecycleState, LlmCallBegin, LlmCallInputs, Span, SpanKind, Step, StepKind,
        };

        fn user_msg(text: &str) -> ChatMessage {
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text(text.into())],
                from_user: false,
            }
        }
        fn system_msg(text: &str) -> ChatMessage {
            ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(text.into())],
                from_user: false,
            }
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
            .start_job(
                s.id.clone(),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lifecycle.start(&j1.id).await.unwrap();

        let trace_store: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());
        let step1_id = StepId::new();
        let now = Utc::now();
        trace_store
            .save_step(&Step {
                id: step1_id,
                job_id: j1.id,
                kind: StepKind::LlmIteration,
                started_at: now,
                ended_at: None,
                outcome: LifecycleState::Pending,
            })
            .await
            .unwrap();
        let span1_id = SpanId::new();
        trace_store
            .save_span(&Span {
                id: span1_id,
                step_id: step1_id,
                kind: SpanKind::LlmCall {
                    begin: LlmCallBegin {
                        model_id: "claude".into(),
                        provider: "anthropic".into(),
                        provider_config_hash: String::new(),
                        input_messages: LlmCallInputs::Persisted {
                            last_ordinal: pre_last,
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
            })
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
            .start_job(
                s.id.clone(),
                TriggerKind::User,
                user_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lifecycle.start(&j2.id).await.unwrap();

        let step2_id = StepId::new();
        trace_store
            .save_step(&Step {
                id: step2_id,
                job_id: j2.id,
                kind: StepKind::LlmIteration,
                started_at: Utc::now(),
                ended_at: None,
                outcome: LifecycleState::Pending,
            })
            .await
            .unwrap();
        let span2_id = SpanId::new();
        trace_store
            .save_span(&Span {
                id: span2_id,
                step_id: step2_id,
                kind: SpanKind::LlmCall {
                    begin: LlmCallBegin {
                        model_id: "claude".into(),
                        provider: "anthropic".into(),
                        provider_config_hash: String::new(),
                        input_messages: LlmCallInputs::Persisted {
                            last_ordinal: post_last,
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
            })
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
}
