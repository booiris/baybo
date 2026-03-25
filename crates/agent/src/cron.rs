use std::collections::HashMap;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::actor::AgentMessage;
use crate::supervisor::AgentSupervisor;

/// A cron job definition.
#[derive(Debug, Clone)]
pub struct CronJob {
    pub id: String,
    pub session_id: String,
    pub schedule: CronSchedule,
    pub prompt: String,
    pub enabled: bool,
}

/// Cron schedule representation.
#[derive(Debug, Clone)]
pub enum CronSchedule {
    /// Fixed interval between executions.
    Interval(std::time::Duration),
    /// Cron expression string (for future parsing).
    Expression(String),
}

/// Manages cron job scheduling and triggering.
pub struct CronScheduler {
    jobs: HashMap<String, CronJob>,
}

impl CronScheduler {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
        }
    }

    pub fn register(&mut self, job: CronJob) {
        info!(job_id = %job.id, session_id = %job.session_id, "registering cron job");
        self.jobs.insert(job.id.clone(), job);
    }

    pub fn unregister(&mut self, job_id: &str) -> Option<CronJob> {
        debug!(job_id, "unregistering cron job");
        self.jobs.remove(job_id)
    }

    pub fn get(&self, job_id: &str) -> Option<&CronJob> {
        self.jobs.get(job_id)
    }

    pub fn list(&self) -> Vec<&CronJob> {
        self.jobs.values().collect()
    }

    /// Trigger a cron job by routing a CronTrigger message to the appropriate session actor.
    pub async fn trigger(&self, job_id: &str, supervisor: &AgentSupervisor) -> bool {
        let job = match self.jobs.get(job_id) {
            Some(j) if j.enabled => j,
            Some(_) => {
                debug!(job_id, "cron job is disabled, skipping trigger");
                return false;
            }
            None => {
                warn!(job_id, "attempted to trigger unknown cron job");
                return false;
            }
        };

        supervisor
            .route(
                &job.session_id,
                AgentMessage::CronTrigger {
                    job_id: job_id.to_string(),
                },
            )
            .await
    }

    /// Start background tasks that trigger interval-based cron jobs.
    /// Returns handles that can be used to abort the spawned tasks.
    pub fn spawn_interval_jobs(
        &self,
        supervisor: Arc<AgentSupervisor>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();

        for job in self.jobs.values() {
            if !job.enabled {
                continue;
            }
            let interval = match &job.schedule {
                CronSchedule::Interval(d) => *d,
                CronSchedule::Expression(_) => {
                    debug!(job_id = %job.id, "expression-based cron not yet supported");
                    continue;
                }
            };

            let job_id = job.id.clone();
            let session_id = job.session_id.clone();
            let supervisor = Arc::clone(&supervisor);

            let handle = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // skip initial immediate tick
                loop {
                    ticker.tick().await;
                    debug!(job_id = %job_id, "cron interval tick");
                    let routed = supervisor
                        .route(
                            &session_id,
                            AgentMessage::CronTrigger {
                                job_id: job_id.clone(),
                            },
                        )
                        .await;
                    if !routed {
                        warn!(job_id = %job_id, "failed to route cron trigger, no actor found");
                    }
                }
            });
            handles.push(handle);
        }

        handles
    }
}

impl Default for CronScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_job(id: &str, enabled: bool) -> CronJob {
        CronJob {
            id: id.to_string(),
            session_id: "session-1".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(60)),
            prompt: "do something".to_string(),
            enabled,
        }
    }

    #[test]
    fn test_register_and_get() {
        let mut scheduler = CronScheduler::new();
        scheduler.register(make_job("job-1", true));

        let job = scheduler.get("job-1");
        assert!(job.is_some());
        assert_eq!(job.unwrap().id, "job-1");

        assert!(scheduler.get("nonexistent").is_none());
    }

    #[test]
    fn test_unregister() {
        let mut scheduler = CronScheduler::new();
        scheduler.register(make_job("job-1", true));
        assert!(scheduler.get("job-1").is_some());

        let removed = scheduler.unregister("job-1");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id, "job-1");

        assert!(scheduler.get("job-1").is_none());
        assert!(scheduler.unregister("job-1").is_none());
    }

    #[test]
    fn test_list() {
        let mut scheduler = CronScheduler::new();
        assert!(scheduler.list().is_empty());

        scheduler.register(make_job("job-1", true));
        scheduler.register(make_job("job-2", false));
        assert_eq!(scheduler.list().len(), 2);
    }

    #[test]
    fn test_default() {
        let scheduler = CronScheduler::default();
        assert!(scheduler.list().is_empty());
    }
}
