//! In-memory `JobStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `aura-job` (next to the row conversions it
//! pairs with) so crates that depend on `aura-job` can spin up a fake
//! store without pulling the libsql adapter.

use std::collections::HashMap;

use async_trait::async_trait;
use aura_model::{JobId, SessionId};
use aura_store::job::Result;
use aura_store::{JobRow, JobStore, JobTransitionRow};
use parking_lot::Mutex;

/// In-memory `JobStore` for tests. Keyed by `row.id`. `record_transition`
/// appends to a per-job vector so the order of transitions is preserved.
#[derive(Debug, Default)]
pub struct MemoryJobStore {
    jobs: Mutex<HashMap<JobId, JobRow>>,
    transitions: Mutex<HashMap<JobId, Vec<JobTransitionRow>>>,
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
    async fn create(&self, job: &JobRow) -> Result<()> {
        self.jobs.lock().insert(job.id, job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &JobId) -> Result<Option<JobRow>> {
        Ok(self.jobs.lock().get(job_id).cloned())
    }

    async fn save(&self, job: &JobRow) -> Result<()> {
        self.jobs.lock().insert(job.id, job.clone());
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| &j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_active_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| {
                &j.session_id == session_id
                    && matches!(j.status_kind.as_str(), "pending" | "in_progress" | "stuck")
            })
            .cloned()
            .collect())
    }

    async fn list_by_status_kind(&self, status_kind: &str) -> Result<Vec<JobRow>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status_kind == status_kind)
            .cloned()
            .collect())
    }

    async fn list_recoverable(&self) -> Result<Vec<JobRow>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| matches!(j.status_kind.as_str(), "pending" | "in_progress" | "stuck"))
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_job_id: &JobId) -> Result<Vec<JobRow>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.parent_job_id.as_ref() == Some(parent_job_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<JobRow>> {
        Ok(self.jobs.lock().values().cloned().collect())
    }

    async fn record_transition(&self, transition: &JobTransitionRow) -> Result<()> {
        self.transitions
            .lock()
            .entry(transition.job_id)
            .or_default()
            .push(transition.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &JobId) -> Result<Vec<JobTransitionRow>> {
        Ok(self
            .transitions
            .lock()
            .get(job_id)
            .cloned()
            .unwrap_or_default())
    }
}
