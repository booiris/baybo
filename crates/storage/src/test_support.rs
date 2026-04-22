//! In-memory store implementations for downstream crates' tests.
//!
//! Gated behind the `test-support` cargo feature so they never ship in
//! release builds. Add new fakes here as the trait surface grows; keep
//! each fake colocated with the trait it implements (in this crate's
//! sibling modules) so changing the trait forces an update.

use std::collections::HashMap;

use async_trait::async_trait;
use aura_job::{Job, JobStatus, JobTransition};
use aura_model::MemoryEntry;
use aura_trace::{SessionTrace, TraceFilter, TraceNode, TraceNodeId};
use parking_lot::Mutex;

use crate::cost::{CostRecord, CostResult, CostStore, CostSummary, TimeRange};
use crate::job::{JobStore, Result as JobStoreResult};
use crate::memory::{MemoryStore, Result as MemoryStoreResult};
use crate::secret::{Result as SecretResult, SecretStore};
use crate::trace::{Result as TraceStoreResult, TraceStore};

/// In-memory `SecretStore` for tests. Stores raw `(name, encrypted_value)`
/// pairs in a `Mutex<HashMap>`. No encryption performed here — the bytes
/// are whatever the caller hands in (typically already AES-GCM ciphertext
/// from `SecretVault`).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    data: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live entries. Useful for asserting deterministic-mint
    /// invariants ("same secret minted twice → vault holds one entry").
    pub fn len(&self) -> usize {
        self.data.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> SecretResult<()> {
        self.data
            .lock()
            .insert(name.to_owned(), encrypted_value.to_vec());
        Ok(())
    }

    async fn retrieve(&self, name: &str) -> SecretResult<Option<Vec<u8>>> {
        Ok(self.data.lock().get(name).cloned())
    }

    async fn list(&self) -> SecretResult<Vec<String>> {
        Ok(self.data.lock().keys().cloned().collect())
    }

    async fn delete(&self, name: &str) -> SecretResult<()> {
        self.data.lock().remove(name);
        Ok(())
    }
}

/// In-memory `JobStore` for tests. Keyed by `job.id`. `record_transition`
/// appends to a per-job vector so the order of transitions is preserved.
#[derive(Debug, Default)]
pub struct MemoryJobStore {
    jobs: Mutex<HashMap<String, Job>>,
    transitions: Mutex<HashMap<String, Vec<JobTransition>>>,
}

impl MemoryJobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.jobs.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl JobStore for MemoryJobStore {
    async fn create(&self, job: &Job) -> JobStoreResult<()> {
        self.jobs.lock().insert(job.id.clone(), job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &str) -> JobStoreResult<Option<Job>> {
        Ok(self.jobs.lock().get(job_id).cloned())
    }

    async fn save(&self, job: &Job) -> JobStoreResult<()> {
        self.jobs.lock().insert(job.id.clone(), job.clone());
        Ok(())
    }

    async fn list_by_session(&self, session_id: &str) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_by_status(&self, status: JobStatus) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status == status)
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_job_id: &str) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> JobStoreResult<Vec<Job>> {
        Ok(self.jobs.lock().values().cloned().collect())
    }

    async fn record_transition(&self, transition: &JobTransition) -> JobStoreResult<()> {
        self.transitions
            .lock()
            .entry(transition.job_id.clone())
            .or_default()
            .push(transition.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &str) -> JobStoreResult<Vec<JobTransition>> {
        Ok(self
            .transitions
            .lock()
            .get(job_id)
            .cloned()
            .unwrap_or_default())
    }
}

/// In-memory `CostStore` for tests. Records are appended in arrival
/// order; queries scan linearly. Plenty fast for tests.
#[derive(Debug, Default)]
pub struct MemoryCostStore {
    records: Mutex<Vec<CostRecord>>,
}

impl MemoryCostStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn in_range(record: &CostRecord, range: &TimeRange) -> bool {
    record.timestamp >= range.from && record.timestamp < range.to
}

#[async_trait]
impl CostStore for MemoryCostStore {
    async fn record(&self, record: &CostRecord) -> CostResult<()> {
        self.records.lock().push(record.clone());
        Ok(())
    }

    async fn query_user(&self, user_id: &str, range: TimeRange) -> CostResult<Vec<CostRecord>> {
        Ok(self
            .records
            .lock()
            .iter()
            .filter(|r| r.user_id == user_id && in_range(r, &range))
            .cloned()
            .collect())
    }

    async fn query_global(&self, range: TimeRange) -> CostResult<CostSummary> {
        let mut summary = CostSummary::default();
        for r in self.records.lock().iter().filter(|r| in_range(r, &range)) {
            summary.total_cost_usd += r.cost_usd;
            summary.total_input_tokens += r.input_tokens;
            summary.total_output_tokens += r.output_tokens;
            summary.record_count += 1;
        }
        Ok(summary)
    }

    async fn sum_user(&self, user_id: &str, range: TimeRange) -> CostResult<f64> {
        Ok(self
            .records
            .lock()
            .iter()
            .filter(|r| r.user_id == user_id && in_range(r, &range))
            .map(|r| r.cost_usd)
            .sum())
    }
}

/// In-memory `TraceStore` for tests. Keyed by `session_id`; `save_trace`
/// overwrites prior state for that session (mirrors libsql upsert
/// semantics from the assembly layer's perspective).
#[derive(Debug, Default)]
pub struct MemoryTraceStore {
    traces: Mutex<HashMap<String, SessionTrace>>,
}

impl MemoryTraceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.traces.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TraceStore for MemoryTraceStore {
    async fn save_trace(&self, trace: &SessionTrace) -> TraceStoreResult<()> {
        self.traces
            .lock()
            .insert(trace.session_id.clone(), trace.clone());
        Ok(())
    }

    async fn load_trace(&self, session_id: &str) -> TraceStoreResult<Option<SessionTrace>> {
        Ok(self.traces.lock().get(session_id).cloned())
    }

    async fn query_traces(&self, filter: TraceFilter) -> TraceStoreResult<Vec<SessionTrace>> {
        let traces = self.traces.lock();
        Ok(traces
            .values()
            .filter(|t| {
                filter
                    .session_id
                    .as_deref()
                    .is_none_or(|sid| t.session_id == sid)
            })
            .cloned()
            .collect())
    }

    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> TraceStoreResult<Option<TraceNode>> {
        Ok(self
            .traces
            .lock()
            .get(session_id)
            .and_then(|t| t.nodes.get(node_id).cloned()))
    }
}

/// In-memory `MemoryStore` for tests. Keyed by `entry.id`. Search is a
/// case-insensitive substring match against `key + value` — good enough
/// for asserting "the entry I just stored shows up in search".
#[derive(Debug, Default)]
pub struct MemoryMemoryStore {
    entries: Mutex<HashMap<String, MemoryEntry>>,
}

impl MemoryMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl MemoryStore for MemoryMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> MemoryStoreResult<()> {
        self.entries.lock().insert(entry.id.clone(), entry.clone());
        Ok(())
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> MemoryStoreResult<Option<MemoryEntry>> {
        Ok(self
            .entries
            .lock()
            .values()
            .find(|e| e.user_id == user_id && e.id == key)
            .cloned())
    }

    async fn search(
        &self,
        user_id: &str,
        query: &str,
        limit: usize,
    ) -> MemoryStoreResult<Vec<MemoryEntry>> {
        let q = query.to_ascii_lowercase();
        Ok(self
            .entries
            .lock()
            .values()
            .filter(|e| e.user_id == user_id && e.content.to_ascii_lowercase().contains(&q))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn delete(&self, id: &str) -> MemoryStoreResult<()> {
        self.entries.lock().remove(id);
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> MemoryStoreResult<Vec<MemoryEntry>> {
        Ok(self
            .entries
            .lock()
            .values()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> MemoryStoreResult<Vec<MemoryEntry>> {
        Ok(self.entries.lock().values().cloned().collect())
    }

    async fn get_by_id(&self, id: &str) -> MemoryStoreResult<Option<MemoryEntry>> {
        Ok(self.entries.lock().get(id).cloned())
    }
}
