//! `JobInputKind`, `JobInput`, `JobOutput`.
//!
//! A job carries three orthogonal descriptors, each with a single
//! source of truth:
//! - **input kind** ([`JobInputKind`]) — what payload fed it; a
//!   projection of [`JobInput`]. Display / denormalisation only.
//! - **origin** ([`baybo_model::TriggerKind`], stored on `Job`) — the
//!   owning session's root trigger, recorded as-is at creation. Not
//!   asserted against the payload: a maintenance job runs inside a
//!   `User`-trigger session and records `origin = User` honestly.

use baybo_model::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What payload fed this job — a projection of [`JobInput`]. Used for
/// display and the denormalised `jobs.kind` column only; behaviour
/// branches on [`JobInput`] itself, never on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobInputKind {
    UserChat,
    Cron,
    Compact,
    Spawned,
    SubagentNotification,
}

/// What initially fed this job.
///
/// `Cron::action_payload` is an opaque trace blob written by the cron
/// router (currently `{cron_job_id, prompt}`). Kept as `Value` here so
/// `baybo-job` does not depend on `baybo-cron`, which would invert the
/// layer order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobInput {
    UserChat {
        content: Vec<ContentBlock>,
    },
    Cron {
        action_payload: Value,
    },
    /// User-requested foreground compaction (`/compact`). It opens a real job
    /// so compression steps and spans have lifecycle, but it is not a chat
    /// turn and must not drive TurnState or push notifications.
    Compact,
    Spawned {
        initial_prompt: Vec<ContentBlock>,
    },
    /// The parent session's autonomous turn reacting to one or more
    /// finished background subagents. Allowed under any root trigger
    /// (like `Spawned`); `content` is the synthesized XML notification.
    SubagentNotification {
        content: Vec<ContentBlock>,
    },
}

impl JobInput {
    pub fn input_kind(&self) -> JobInputKind {
        match self {
            JobInput::UserChat { .. } => JobInputKind::UserChat,
            JobInput::Cron { .. } => JobInputKind::Cron,
            JobInput::Compact => JobInputKind::Compact,
            JobInput::Spawned { .. } => JobInputKind::Spawned,
            JobInput::SubagentNotification { .. } => JobInputKind::SubagentNotification,
        }
    }
}

/// What the job produced as its contractual output (the value that
/// acceptance / review workflows judge against).
///
/// Progress messages emitted **mid-job** do not live here — they are in
/// the trace tree, indexed by `Job.emitted_span_ids`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOutput {
    /// The final user-facing reply (for chat-style jobs). `ordinal` is the
    /// persisted `session_messages.ordinal` of that reply row, captured from the
    /// store append; it rides the `Completed` lifecycle event so the push
    /// dispatcher reads exactly this reply without a read-after-write poll.
    /// `None` only when the turn ran with no durable store (ephemeral/test) or
    /// for a legacy row persisted before this field existed.
    Message {
        content: Vec<ContentBlock>,
        #[serde(default)]
        ordinal: Option<i64>,
    },
    /// Structured payload (for tool-direct cron jobs, maintenance jobs whose
    /// product is structured data, etc.).
    Structured { value: Value },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_kind_matches_variant() {
        let i = JobInput::UserChat { content: vec![] };
        assert_eq!(i.input_kind(), JobInputKind::UserChat);

        let i = JobInput::Cron {
            action_payload: serde_json::json!({}),
        };
        assert_eq!(i.input_kind(), JobInputKind::Cron);

        let i = JobInput::Compact;
        assert_eq!(i.input_kind(), JobInputKind::Compact);

        let i = JobInput::Spawned {
            initial_prompt: vec![],
        };
        assert_eq!(i.input_kind(), JobInputKind::Spawned);

        let i = JobInput::SubagentNotification { content: vec![] };
        assert_eq!(i.input_kind(), JobInputKind::SubagentNotification);
    }

    #[test]
    fn input_round_trips_through_serde() {
        for input in [
            JobInput::UserChat { content: vec![] },
            JobInput::Cron {
                action_payload: serde_json::json!({"cron_job_id": "c1", "prompt": "run"}),
            },
            JobInput::Compact,
            JobInput::Spawned {
                initial_prompt: vec![ContentBlock::Text("task".into())],
            },
            JobInput::SubagentNotification { content: vec![] },
        ] {
            let s = serde_json::to_string(&input).unwrap();
            let back: JobInput = serde_json::from_str(&s).unwrap();
            assert_eq!(back.input_kind(), input.input_kind());
        }
    }

    #[test]
    fn output_round_trips_through_serde() {
        let o = JobOutput::Message {
            content: vec![],
            ordinal: None,
        };
        let s = serde_json::to_string(&o).unwrap();
        let _: JobOutput = serde_json::from_str(&s).unwrap();
    }
}
