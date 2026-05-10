//! `JobKind`, `JobInput`, `JobOutput`.
//!
//! Trigger ↔ Job kind mapping (the `session.trigger.kind() == job.kind()`
//! invariant in `session.md`):
//!
//! | session trigger | allowed job kind            |
//! | --------------- | --------------------------- |
//! | `User`          | `JobKind::UserChat`         |
//! | `Cron`          | `JobKind::Cron`             |
//! | `System`        | `JobKind::System`           |
//! | (any)           | `JobKind::Spawned` is also valid in any session — spawned jobs inherit their parent context regardless of root trigger |

use aura_model::{ContentBlock, SystemTrigger, TriggerKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Discriminator for the four job shapes. Mirrors `TriggerKind` 1:1
/// plus `Spawned` (subagent / user-fork-continuation jobs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    UserChat,
    Cron,
    System,
    Spawned,
}

impl JobKind {
    /// Whether this kind of job is permitted on a session whose root
    /// trigger has the given kind. `Spawned` is permitted everywhere.
    pub fn allowed_for(&self, trigger: TriggerKind) -> bool {
        matches!(
            (self, trigger),
            (JobKind::UserChat, TriggerKind::User)
                | (JobKind::Cron, TriggerKind::Cron)
                | (JobKind::System, TriggerKind::System)
                | (JobKind::Spawned, _)
        )
    }
}

/// What initially fed this job.
///
/// `Cron::action_payload` is an opaque trace blob written by the cron
/// router (currently `{cron_job_id, prompt}`). Kept as `Value` here so
/// `aura-job` does not depend on `aura-cron`, which would invert the
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
    /// A maintenance task — see [`SystemTrigger`] for the variants.
    /// The trigger carries its payload directly; no opaque
    /// `serde_json::Value` blob.
    System {
        trigger: SystemTrigger,
    },
    Spawned {
        initial_prompt: Vec<ContentBlock>,
    },
}

impl JobInput {
    pub fn kind(&self) -> JobKind {
        match self {
            JobInput::UserChat { .. } => JobKind::UserChat,
            JobInput::Cron { .. } => JobKind::Cron,
            JobInput::System { .. } => JobKind::System,
            JobInput::Spawned { .. } => JobKind::Spawned,
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
    /// The final user-facing reply (for chat-style jobs).
    Message { content: Vec<ContentBlock> },
    /// Structured payload (for tool-direct cron jobs, system jobs whose
    /// product is structured data, etc.).
    Structured { value: Value },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_kind_allowed_for_matches_trigger() {
        assert!(JobKind::UserChat.allowed_for(TriggerKind::User));
        assert!(JobKind::Cron.allowed_for(TriggerKind::Cron));
        assert!(JobKind::System.allowed_for(TriggerKind::System));
        // Spawned is allowed under any root trigger
        assert!(JobKind::Spawned.allowed_for(TriggerKind::User));
        assert!(JobKind::Spawned.allowed_for(TriggerKind::Cron));
        assert!(JobKind::Spawned.allowed_for(TriggerKind::System));
        // Mismatches are rejected
        assert!(!JobKind::UserChat.allowed_for(TriggerKind::Cron));
        assert!(!JobKind::Cron.allowed_for(TriggerKind::User));
        assert!(!JobKind::System.allowed_for(TriggerKind::User));
    }

    #[test]
    fn input_kind_matches_variant() {
        let i = JobInput::UserChat { content: vec![] };
        assert_eq!(i.kind(), JobKind::UserChat);

        let i = JobInput::Cron {
            action_payload: serde_json::json!({}),
        };
        assert_eq!(i.kind(), JobKind::Cron);

        let i = JobInput::System {
            trigger: SystemTrigger::HistoryReview,
        };
        assert_eq!(i.kind(), JobKind::System);

        let i = JobInput::Spawned {
            initial_prompt: vec![],
        };
        assert_eq!(i.kind(), JobKind::Spawned);
    }

    #[test]
    fn input_round_trips_through_serde() {
        let i = JobInput::System {
            trigger: SystemTrigger::MemoryConsolidation,
        };
        let s = serde_json::to_string(&i).unwrap();
        let back: JobInput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind(), i.kind());
    }

    #[test]
    fn output_round_trips_through_serde() {
        let o = JobOutput::Message { content: vec![] };
        let s = serde_json::to_string(&o).unwrap();
        let _: JobOutput = serde_json::from_str(&s).unwrap();
    }
}
