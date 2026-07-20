//! In-memory `JobStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `baybo-job` (next to the row conversions it
//! pairs with) so crates that depend on `baybo-job` can spin up a fake
//! store without pulling the sqlite adapter.

use std::collections::HashMap;

use async_trait::async_trait;
use baybo_model::{JobId, SessionId};
use baybo_store::job::Result;
use baybo_store::{JobRow, JobStore, SessionJobStats};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// In-memory `JobStore` for tests. Keyed by `row.id`.
#[derive(Debug, Default)]
pub struct MemoryJobStore {
    jobs: Mutex<HashMap<JobId, JobRow>>,
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

    async fn list_page(
        &self,
        status_kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<JobRow>, usize)> {
        let mut rows: Vec<JobRow> = self
            .jobs
            .lock()
            .values()
            .filter(|j| status_kind.is_none_or(|k| j.status_kind == k))
            .cloned()
            .collect();
        rows.sort_by_key(|j| std::cmp::Reverse(j.created_at));
        let total = rows.len();
        let page = rows.into_iter().skip(offset).take(limit).collect();
        Ok((page, total))
    }

    async fn count_by_status_kind(&self, status_kind: &str) -> Result<usize> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status_kind == status_kind)
            .count())
    }

    async fn session_job_stats(&self) -> Result<Vec<SessionJobStats>> {
        let mut by_session: HashMap<SessionId, (usize, DateTime<Utc>, String)> = HashMap::new();
        for j in self.jobs.lock().values() {
            let entry = by_session.entry(j.session_id.clone()).or_insert((
                0,
                j.created_at,
                j.status_kind.clone(),
            ));
            entry.0 += 1;
            if j.created_at >= entry.1 {
                entry.1 = j.created_at;
                entry.2 = j.status_kind.clone();
            }
        }
        Ok(by_session
            .into_iter()
            .map(
                |(session_id, (job_count, _, latest_status_kind))| SessionJobStats {
                    session_id,
                    job_count,
                    latest_status_kind,
                },
            )
            .collect())
    }
}
