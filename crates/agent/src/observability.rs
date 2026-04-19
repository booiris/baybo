use std::sync::Arc;

use crate::cost::CostTracker;
use crate::job::JobManager;
use crate::trace::TraceCollector;
use aura_context::ContextSnapshot;
use aura_job::OperationKind;
use aura_storage::CostRecord;
use aura_trace::{ExecutionProvenance, SpanHandle, SpanInput, SpanResult, TraceNodeId};
use chrono::Utc;
use parking_lot::Mutex;
use tracing::warn;

/// Unified wrapper for Job + Trace + Cost recording.
///
/// Business code should use short-lived `begin/succeed/fail` calls
/// and never hold this across long awaits.
pub struct ObservabilityRecorder {
    job_manager: Arc<JobManager>,
    trace_collector: Arc<Mutex<TraceCollector>>,
    cost_tracker: Arc<CostTracker>,
}

/// Handle for an in-flight operation being recorded.
pub struct OperationHandle {
    pub job_id: String,
    pub span_handle: SpanHandle,
}

impl ObservabilityRecorder {
    pub fn new(
        job_manager: Arc<JobManager>,
        trace_collector: Arc<Mutex<TraceCollector>>,
        cost_tracker: Arc<CostTracker>,
    ) -> Self {
        Self {
            job_manager,
            trace_collector,
            cost_tracker,
        }
    }

    /// Begin recording an operation: create a Job and a Trace span.
    pub async fn begin(
        &self,
        session_id: &str,
        kind: OperationKind,
        parent_job: Option<&str>,
        provenance: ExecutionProvenance,
        input: SpanInput,
    ) -> anyhow::Result<OperationHandle> {
        let job = self
            .job_manager
            .create_job(session_id, kind.clone(), parent_job)
            .await?;
        self.job_manager.start(&job.id).await?;

        let span_handle = {
            let mut collector = self.trace_collector.lock();
            collector.begin_span(kind, Some(&job.id), provenance, input)
        };

        Ok(OperationHandle {
            job_id: job.id,
            span_handle,
        })
    }

    /// Record a successful completion.
    pub async fn succeed(
        &self,
        handle: OperationHandle,
        output: serde_json::Value,
        result: SpanResult,
    ) -> anyhow::Result<()> {
        {
            let mut collector = self.trace_collector.lock();
            collector.end_span(handle.span_handle, result);
        }
        self.job_manager.complete(&handle.job_id, output).await?;
        self.job_manager.submit(&handle.job_id).await?;
        self.job_manager.accept(&handle.job_id).await?;
        Ok(())
    }

    /// Record a failure.
    pub async fn fail(&self, handle: OperationHandle, error: &str) -> anyhow::Result<()> {
        {
            let mut collector = self.trace_collector.lock();
            collector.end_span(
                handle.span_handle,
                SpanResult::Error {
                    error: error.to_string(),
                },
            );
        }
        self.job_manager.fail(&handle.job_id, error).await?;
        Ok(())
    }

    /// Record an LLM cost.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_cost(
        &self,
        user_id: &str,
        session_id: &str,
        job_id: &str,
        trace_span_id: &str,
        model: &str,
        input_tokens: usize,
        output_tokens: usize,
        cost_usd: f64,
    ) {
        let record = CostRecord {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            job_id: job_id.to_string(),
            trace_span_id: trace_span_id.to_string(),
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cost_usd,
            timestamp: Utc::now(),
        };
        if let Err(e) = self.cost_tracker.record(&record).await {
            warn!(error = %e, "failed to record cost");
        }
    }

    /// Flush trace data to the store.
    pub async fn flush(&self) -> anyhow::Result<()> {
        let (store, trace) = {
            let collector = self.trace_collector.lock();
            collector.flush_snapshot()
        };
        store.save_trace(&trace).await?;
        Ok(())
    }

    /// Check whether the auto-snapshot policy says a snapshot should be taken now.
    ///
    /// Returns the active leaf node id when a snapshot is due, so the caller
    /// can create a `ContextSnapshot` and pass it to `attach_snapshot`.
    pub async fn maybe_snapshot(&self) -> Option<TraceNodeId> {
        let collector = self.trace_collector.lock();
        if collector.should_auto_snapshot() {
            Some(collector.active_leaf().clone())
        } else {
            None
        }
    }

    /// Attach a context snapshot to the specified trace node.
    pub async fn attach_snapshot(
        &self,
        node_id: &TraceNodeId,
        snapshot: ContextSnapshot,
    ) -> anyhow::Result<()> {
        let mut collector = self.trace_collector.lock();
        Ok(collector.attach_snapshot(node_id, snapshot)?)
    }

    /// Find the nearest context snapshot at or above `target_node`, then fork
    /// the trace tree so subsequent spans are recorded on a new branch.
    ///
    /// Returns the snapshot that was found.  The caller uses it to restore
    /// the session's message history and context budget.
    pub async fn rollback_to(&self, target_node: &TraceNodeId) -> anyhow::Result<ContextSnapshot> {
        let mut collector = self.trace_collector.lock();
        let snapshot = collector.find_snapshot_at(target_node)?;
        collector.fork_from(target_node.clone())?;
        Ok(snapshot)
    }
}
