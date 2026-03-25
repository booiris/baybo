use std::sync::Arc;
use std::time::Duration;

use aura_workspace::WorkspaceManager;
use aura_workspace::heartbeat::{HeartbeatSpec, RoutineSchedule};
use tracing::{debug, info, warn};

use crate::actor::AgentMessage;
use crate::supervisor::AgentSupervisor;

/// Runs periodic heartbeat ticks and dispatches them to agents.
pub struct HeartbeatRunner {
    interval: Duration,
    _workspace: Arc<WorkspaceManager>,
}

impl HeartbeatRunner {
    pub fn new(interval: Duration, workspace: Arc<WorkspaceManager>) -> Self {
        Self {
            interval,
            _workspace: workspace,
        }
    }

    /// Start the heartbeat loop. Sends HeartbeatTick to all active agents at the configured interval.
    pub fn spawn(self, supervisor: Arc<AgentSupervisor>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            loop {
                ticker.tick().await;
                debug!("heartbeat tick");
                supervisor.broadcast(AgentMessage::HeartbeatTick).await;
            }
        })
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }
}

/// Definition of a routine to be scheduled.
#[derive(Debug, Clone)]
pub struct RoutineDefinition {
    pub id: String,
    pub schedule: RoutineSchedule,
    pub prompt: String,
    pub notify_channel: Option<String>,
    pub session_id: String,
}

/// Schedules routines from workspace HeartbeatSpec and dispatches them to agents.
pub struct RoutineScheduler {
    routines: Vec<RoutineDefinition>,
}

impl RoutineScheduler {
    pub fn new() -> Self {
        Self {
            routines: Vec::new(),
        }
    }

    /// Load routine definitions from a HeartbeatSpec.
    pub fn load_from_spec(&mut self, spec: &HeartbeatSpec, default_session_id: &str) {
        for routine_spec in &spec.routines {
            let def = RoutineDefinition {
                id: routine_spec.id.clone(),
                schedule: routine_spec.schedule.clone(),
                prompt: routine_spec.prompt.clone(),
                notify_channel: routine_spec.notify_channel.clone(),
                session_id: default_session_id.to_string(),
            };
            info!(routine_id = %def.id, "loaded routine definition");
            self.routines.push(def);
        }
    }

    pub fn routines(&self) -> &[RoutineDefinition] {
        &self.routines
    }

    /// Spawn background tasks for interval-based routines.
    pub fn spawn_interval_routines(
        &self,
        supervisor: Arc<AgentSupervisor>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();

        for routine in &self.routines {
            let interval = match &routine.schedule {
                RoutineSchedule::Interval(d) => *d,
                RoutineSchedule::Cron(_) => {
                    debug!(routine_id = %routine.id, "cron-based routine not yet supported");
                    continue;
                }
            };

            let routine_id = routine.id.clone();
            let session_id = routine.session_id.clone();
            let supervisor = Arc::clone(&supervisor);

            let handle = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // skip initial immediate tick
                loop {
                    ticker.tick().await;
                    debug!(routine_id = %routine_id, "routine interval tick");
                    let routed = supervisor
                        .route(
                            &session_id,
                            AgentMessage::RoutineTrigger {
                                routine_id: routine_id.clone(),
                            },
                        )
                        .await;
                    if !routed {
                        warn!(routine_id = %routine_id, "failed to route routine trigger");
                    }
                }
            });
            handles.push(handle);
        }

        handles
    }
}

impl Default for RoutineScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_workspace::heartbeat::{HeartbeatSpec, RoutineSchedule, RoutineSpec};

    #[test]
    fn test_load_from_spec_interval() {
        let spec = HeartbeatSpec {
            interval: Duration::from_secs(60),
            routines: vec![RoutineSpec {
                id: "health-check".to_string(),
                schedule: RoutineSchedule::Interval(Duration::from_secs(30)),
                prompt: "check system health".to_string(),
                notify_channel: Some("ops".to_string()),
            }],
        };

        let mut scheduler = RoutineScheduler::new();
        scheduler.load_from_spec(&spec, "default-session");

        assert_eq!(scheduler.routines().len(), 1);
        let routine = &scheduler.routines()[0];
        assert_eq!(routine.id, "health-check");
        assert_eq!(routine.prompt, "check system health");
        assert_eq!(routine.notify_channel.as_deref(), Some("ops"));
        assert_eq!(routine.session_id, "default-session");
        assert!(matches!(
            routine.schedule,
            RoutineSchedule::Interval(d) if d == Duration::from_secs(30)
        ));
    }

    #[test]
    fn test_load_from_spec_cron() {
        let spec = HeartbeatSpec {
            interval: Duration::from_secs(60),
            routines: vec![RoutineSpec {
                id: "daily-summary".to_string(),
                schedule: RoutineSchedule::Cron("0 9 * * *".to_string()),
                prompt: "summarize activity".to_string(),
                notify_channel: None,
            }],
        };

        let mut scheduler = RoutineScheduler::new();
        scheduler.load_from_spec(&spec, "sess-1");

        assert_eq!(scheduler.routines().len(), 1);
        let routine = &scheduler.routines()[0];
        assert!(matches!(&routine.schedule, RoutineSchedule::Cron(c) if c == "0 9 * * *"));
        assert!(routine.notify_channel.is_none());
    }

    #[test]
    fn test_load_multiple_routines() {
        let spec = HeartbeatSpec {
            interval: Duration::from_secs(60),
            routines: vec![
                RoutineSpec {
                    id: "r1".to_string(),
                    schedule: RoutineSchedule::Interval(Duration::from_secs(10)),
                    prompt: "first".to_string(),
                    notify_channel: None,
                },
                RoutineSpec {
                    id: "r2".to_string(),
                    schedule: RoutineSchedule::Interval(Duration::from_secs(20)),
                    prompt: "second".to_string(),
                    notify_channel: None,
                },
            ],
        };

        let mut scheduler = RoutineScheduler::new();
        scheduler.load_from_spec(&spec, "sess");

        assert_eq!(scheduler.routines().len(), 2);
    }

    #[test]
    fn test_empty_spec() {
        let spec = HeartbeatSpec {
            interval: Duration::from_secs(60),
            routines: vec![],
        };

        let mut scheduler = RoutineScheduler::new();
        scheduler.load_from_spec(&spec, "sess");

        assert!(scheduler.routines().is_empty());
    }

    #[test]
    fn test_default() {
        let scheduler = RoutineScheduler::default();
        assert!(scheduler.routines().is_empty());
    }
}
