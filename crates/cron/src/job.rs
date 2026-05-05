use aura_model::{ApprovedResource, ChannelType};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Lifecycle state of a cron job row.
///
/// `Executed` is reserved for one-shot (`At`) jobs after their single
/// fire — the row is kept (not deleted) so callers and the web UI can
/// see "this fired" history. Recurring (`Cron`) jobs cycle between
/// `Enabled` and `Disabled` and never enter `Executed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronStatus {
    Enabled,
    Disabled,
    Executed,
}

impl CronStatus {
    /// Stable wire string. Mirrors `serde(rename_all = "snake_case")` so
    /// the storage row, CLI labels, and tool output stay in lockstep
    /// without three independent match ladders that can drift.
    pub fn as_str(&self) -> &'static str {
        match self {
            CronStatus::Enabled => "enabled",
            CronStatus::Disabled => "disabled",
            CronStatus::Executed => "executed",
        }
    }
}

/// When a cron job fires.
///
/// The variant alone determines recurrence: `Cron` expressions always repeat,
/// `At` timestamps always fire exactly once and then the job self-evicts.
/// There is no separate "run mode" — a recurring single timestamp or a
/// one-shot cron expression has no sensible meaning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronSchedule {
    /// Standard cron expression (5/6/7-field), fires on every matching tick.
    Cron { expr: String },
    /// Absolute UTC instant, fires exactly once, then the job is deleted.
    At { time: DateTime<Utc> },
}

impl CronSchedule {
    pub fn cron(expr: impl Into<String>) -> Self {
        Self::Cron { expr: expr.into() }
    }

    pub fn at(time: DateTime<Utc>) -> Self {
        Self::At { time }
    }

    /// True when the schedule fires at most once (i.e. `At`).
    pub fn is_one_shot(&self) -> bool {
        matches!(self, Self::At { .. })
    }

    /// Human-friendly single-line rendering, used by CLI/tool output where
    /// the full typed variant is not meaningful to the reader.
    pub fn display(&self) -> String {
        match self {
            Self::Cron { expr } => expr.clone(),
            Self::At { time } => time.to_rfc3339(),
        }
    }
}

/// What to do when a cron job fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerAction {
    /// Send prompt through the full agent loop (LLM).
    Prompt { prompt: String },
    /// Directly invoke a registered tool. Falls back to LLM on failure.
    ToolCall {
        tool_name: String,
        params: Value,
        /// Resources pre-approved at creation time.
        approved_resources: Vec<ApprovedResource>,
    },
}

// ── CronJob ──────────────────────────────────────────────────────────

/// A persistent cron job definition.
///
/// Bound to `user_id + channel` (not `session_id`) so it survives
/// session expiration. Session is resolved dynamically at trigger time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub user_id: String,
    pub channel: ChannelType,
    /// When this job fires. See [`CronSchedule`] for the two variants.
    pub schedule: CronSchedule,
    /// What to do when the job fires.
    pub action: TriggerAction,
    pub status: CronStatus,
    pub last_triggered_at: Option<DateTime<Utc>>,
    /// Pre-computed next fire time for efficient DB queries. `None` means
    /// the job is disabled or its one-shot time has passed.
    pub next_trigger_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Session where this cron job was created (for traceability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session_id: Option<String>,
}

impl CronJob {
    pub fn is_enabled(&self) -> bool {
        self.status == CronStatus::Enabled
    }

    pub fn is_one_shot(&self) -> bool {
        self.schedule.is_one_shot()
    }
}

// ── ExecutionStatus ──────────────────────────────────────────────────

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

// ── CronExecution ────────────────────────────────────────────────────

/// An immutable record of a single cron job execution.
/// Preserved even after one-shot jobs are evicted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronExecution {
    pub id: String,
    pub job_id: String,
    pub user_id: String,
    pub channel: ChannelType,
    pub schedule: CronSchedule,
    pub action: TriggerAction,
    /// The schedule slot that was due (i.e. the `next_trigger_at` value from the job).
    pub scheduled_fire_time: DateTime<Utc>,
    pub triggered_at: DateTime<Utc>,
    pub status: ExecutionStatus,
    /// The session that originally registered the cron job (if any).
    /// Carried through to the dispatched `CronTriggerEvent` so the
    /// router can set `Lineage` / `TriggerSource` on the resulting
    /// turn — the symmetric counterpart to `create_spawned_session`'s
    /// lineage plumbing for subagents and forks.
    #[serde(default)]
    pub origin_session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_cron() {
        let job = CronJob {
            id: "cj-1".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::cron("0 9 * * *"),
            action: TriggerAction::Prompt {
                prompt: "push news".to_string(),
            },
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: Some("sess-1".to_string()),
        };
        let json = serde_json::to_string(&job).unwrap();
        let restored: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "cj-1");
        assert_eq!(restored.status, CronStatus::Enabled);
        assert!(!restored.is_one_shot());
        assert!(restored.is_enabled());
        assert_eq!(restored.origin_session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn serde_round_trip_at() {
        let fire_at = Utc::now();
        let job = CronJob {
            id: "cj-at".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::at(fire_at),
            action: TriggerAction::Prompt {
                prompt: "one shot".to_string(),
            },
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: Some(fire_at),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let restored: CronJob = serde_json::from_str(&json).unwrap();
        assert!(restored.is_one_shot());
        match restored.schedule {
            CronSchedule::At { time } => assert_eq!(time, fire_at),
            other => panic!("expected At, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_round_trip() {
        let job = CronJob {
            id: "cj-2".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::cron("*/5 * * * *"),
            action: TriggerAction::ToolCall {
                tool_name: "web_fetch".to_string(),
                params: serde_json::json!({"url": "https://example.com"}),
                approved_resources: vec![ApprovedResource::Http {
                    host: aura_model::HostPattern::Exact("example.com".to_string()),
                }],
            },
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let restored: CronJob = serde_json::from_str(&json).unwrap();
        match &restored.action {
            TriggerAction::ToolCall {
                tool_name, params, ..
            } => {
                assert_eq!(tool_name, "web_fetch");
                assert_eq!(params["url"], "https://example.com");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
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
        assert_eq!(
            serde_json::to_string(&CronStatus::Executed).unwrap(),
            "\"executed\""
        );
    }

    #[test]
    fn schedule_display() {
        assert_eq!(CronSchedule::cron("0 9 * * *").display(), "0 9 * * *");
        let ts: DateTime<Utc> = "2026-04-17T14:25:00Z".parse().unwrap();
        assert_eq!(CronSchedule::at(ts).display(), "2026-04-17T14:25:00+00:00");
    }

    #[test]
    fn execution_serde_round_trip() {
        let exec = CronExecution {
            id: "ce-1".to_string(),
            job_id: "cj-1".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::at(Utc::now()),
            action: TriggerAction::Prompt {
                prompt: "push news".to_string(),
            },
            scheduled_fire_time: Utc::now(),
            triggered_at: Utc::now(),
            status: ExecutionStatus::Pending,
            origin_session_id: Some("sess-cron".into()),
        };
        let json = serde_json::to_string(&exec).unwrap();
        let restored: CronExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.job_id, "cj-1");
        assert!(restored.schedule.is_one_shot());
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
