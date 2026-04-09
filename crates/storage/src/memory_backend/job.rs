use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio::sync::RwLock;

use aura_job::{Job, JobError, JobStatus, JobStore, JobTransition, Result};

/// In-memory implementation of [`JobStore`].
pub struct InMemoryJobStore {
    jobs: Arc<RwLock<HashMap<String, Job>>>,
    transitions: Arc<RwLock<Vec<JobTransition>>>,
}

impl InMemoryJobStore {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(HashMap::new())),
            transitions: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl Default for InMemoryJobStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl JobStore for InMemoryJobStore {
    async fn create(&self, job: &Job) -> Result<()> {
        let mut guard = self.jobs.write().await;
        guard.insert(job.id.clone(), job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &str) -> Result<Option<Job>> {
        let guard = self.jobs.read().await;
        Ok(guard.get(job_id).cloned())
    }

    async fn update_status(
        &self,
        job_id: &str,
        status: JobStatus,
        output: Option<Value>,
        error: Option<String>,
    ) -> Result<()> {
        let mut guard = self.jobs.write().await;
        let job = guard
            .get_mut(job_id)
            .ok_or_else(|| JobError::NotFound(format!("job not found: {job_id}")))?;

        if !job.status.can_transition_to(&status) {
            return Err(JobError::InvalidTransition(format!(
                "cannot transition job {} from {} to {}",
                job_id, job.status, status
            )));
        }

        let from = job.status.clone();

        job.status = status.clone();
        if output.is_some() {
            job.output = output;
        }
        if error.is_some() {
            job.error = error.clone();
        }

        // Update timing fields based on the new status.
        let now = Utc::now();
        match status {
            JobStatus::InProgress => {
                if job.started_at.is_none() {
                    job.started_at = Some(now);
                }
            }
            JobStatus::Completed | JobStatus::Failed | JobStatus::Accepted => {
                job.completed_at = Some(now);
            }
            _ => {}
        }

        // Record the transition while we still hold the jobs lock to keep things
        // consistent. We need to drop the jobs guard first though to avoid
        // potential deadlock with transitions lock ordering.
        let transition = JobTransition {
            job_id: job_id.to_string(),
            from,
            to: status,
            reason: error,
            timestamp: now,
        };
        drop(guard);

        let mut t_guard = self.transitions.write().await;
        t_guard.push(transition);

        Ok(())
    }

    async fn list_by_session(&self, session_id: &str) -> Result<Vec<Job>> {
        let guard = self.jobs.read().await;
        let jobs = guard
            .values()
            .filter(|j| j.session_id == session_id)
            .cloned()
            .collect();
        Ok(jobs)
    }

    async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>> {
        let guard = self.jobs.read().await;
        let jobs = guard
            .values()
            .filter(|j| j.status == status)
            .cloned()
            .collect();
        Ok(jobs)
    }

    async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>> {
        let guard = self.jobs.read().await;
        let jobs = guard
            .values()
            .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
            .cloned()
            .collect();
        Ok(jobs)
    }

    async fn record_transition(&self, transition: &JobTransition) -> Result<()> {
        let mut guard = self.transitions.write().await;
        guard.push(transition.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>> {
        let guard = self.transitions.read().await;
        let transitions = guard
            .iter()
            .filter(|t| t.job_id == job_id)
            .cloned()
            .collect();
        Ok(transitions)
    }
}
