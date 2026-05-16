//! In-memory `JobStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `aura-job` (next to the trait it
//! implements) so crates that depend on `aura-job` but not on
//! `aura-storage` can still spin up a fake store for unit tests.

use std::collections::HashMap;

use async_trait::async_trait;
use aura_model::{JobId, SessionId};
use parking_lot::Mutex;

use crate::store::JobStore;
use crate::{Job, JobStatusKind, JobTransition, Result};

/// In-memory `JobStore` for tests. Keyed by `job.id`. `record_transition`
/// appends to a per-job vector so the order of transitions is preserved.
#[derive(Debug, Default)]
pub struct MemoryJobStore {
    jobs: Mutex<HashMap<JobId, Job>>,
    transitions: Mutex<HashMap<JobId, Vec<JobTransition>>>,
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
    async fn create(&self, job: &Job) -> Result<()> {
        self.jobs.lock().insert(job.id, job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &JobId) -> Result<Option<Job>> {
        Ok(self.jobs.lock().get(job_id).cloned())
    }

    async fn save(&self, job: &Job) -> Result<()> {
        self.jobs.lock().insert(job.id, job.clone());
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| &j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_by_status_kind(&self, kind: JobStatusKind) -> Result<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status.kind() == kind)
            .cloned()
            .collect())
    }

    async fn list_recoverable(&self) -> Result<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status.kind().needs_recovery())
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_job_id: &JobId) -> Result<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.parent_job_id.as_ref() == Some(parent_job_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<Job>> {
        Ok(self.jobs.lock().values().cloned().collect())
    }

    async fn record_transition(&self, transition: &JobTransition) -> Result<()> {
        self.transitions
            .lock()
            .entry(transition.job_id)
            .or_default()
            .push(transition.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &JobId) -> Result<Vec<JobTransition>> {
        Ok(self
            .transitions
            .lock()
            .get(job_id)
            .cloned()
            .unwrap_or_default())
    }
}
