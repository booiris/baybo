use aura_session::ChannelType;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronStatus {
    Enabled,
    Disabled,
}

/// Whether a cron job repeats or runs only once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronRunMode {
    /// Fires on every matching schedule tick.
    Recurring,
    /// Fires once, then the job is automatically deleted.
    /// The execution record is preserved for audit.
    OneShot,
}

/// A persistent cron job definition.
///
/// Bound to `user_id + channel` (not `session_id`) so it survives
/// session expiration. Session is resolved dynamically at trigger time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub user_id: String,
    pub channel: ChannelType,
    /// Standard cron expression, e.g. `"0 9 * * *"`.
    pub schedule: String,
    pub prompt: String,
    pub status: CronStatus,
    pub run_mode: CronRunMode,
    pub last_triggered_at: Option<DateTime<Utc>>,
    /// Pre-computed next fire time for efficient DB queries.
    pub next_trigger_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CronJob {
    pub fn is_enabled(&self) -> bool {
        self.status == CronStatus::Enabled
    }

    pub fn is_one_shot(&self) -> bool {
        self.run_mode == CronRunMode::OneShot
    }
}

/// Execution lifecycle status for crash recovery and idempotency.
///
/// `Pending` → execution recorded but trigger not yet dispatched.
/// `Dispatched` → trigger successfully sent to the actor.
///
/// On restart, `Pending` executions are re-dispatched (they crashed
/// between record and send). `Dispatched` executions are left to the
/// Job system's `Stuck` recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Pending,
    Dispatched,
}

/// An immutable record of a single cron job execution.
/// Preserved even after one-shot jobs are evicted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronExecution {
    pub id: String,
    pub job_id: String,
    pub user_id: String,
    pub channel: ChannelType,
    pub schedule: String,
    pub prompt: String,
    pub run_mode: CronRunMode,
    /// The schedule slot that was due (i.e. the `next_trigger_at` value from the job).
    pub scheduled_fire_time: DateTime<Utc>,
    pub triggered_at: DateTime<Utc>,
    pub status: ExecutionStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip() {
        let job = CronJob {
            id: "cj-1".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::Tui,
            schedule: "0 9 * * *".to_string(),
            prompt: "push news".to_string(),
            status: CronStatus::Enabled,
            run_mode: CronRunMode::Recurring,
            last_triggered_at: None,
            next_trigger_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&job).unwrap();
        let restored: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "cj-1");
        assert_eq!(restored.status, CronStatus::Enabled);
        assert_eq!(restored.run_mode, CronRunMode::Recurring);
        assert!(restored.is_enabled());
        assert!(!restored.is_one_shot());
    }

    #[test]
    fn status_serde() {
        assert_eq!(
            serde_json::to_string(&CronStatus::Enabled).unwrap(),
            "\"enabled\""
        );
        assert_eq!(
            serde_json::to_string(&CronStatus::Disabled).unwrap(),
            "\"disabled\""
        );
    }

    #[test]
    fn run_mode_serde() {
        assert_eq!(
            serde_json::to_string(&CronRunMode::Recurring).unwrap(),
            "\"recurring\""
        );
        assert_eq!(
            serde_json::to_string(&CronRunMode::OneShot).unwrap(),
            "\"one_shot\""
        );
    }

    #[test]
    fn execution_serde_round_trip() {
        let exec = CronExecution {
            id: "ce-1".to_string(),
            job_id: "cj-1".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::Tui,
            schedule: "0 9 * * *".to_string(),
            prompt: "push news".to_string(),
            run_mode: CronRunMode::OneShot,
            scheduled_fire_time: Utc::now(),
            triggered_at: Utc::now(),
            status: ExecutionStatus::Pending,
        };
        let json = serde_json::to_string(&exec).unwrap();
        let restored: CronExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.job_id, "cj-1");
        assert_eq!(restored.run_mode, CronRunMode::OneShot);
        assert_eq!(restored.status, ExecutionStatus::Pending);
    }

    #[test]
    fn execution_status_serde() {
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Dispatched).unwrap(),
            "\"dispatched\""
        );
    }
}
