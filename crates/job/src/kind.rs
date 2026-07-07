//! `JobInputKind`, `JobInput`, `JobShape`, `JobOutput`.
//!
//! A job carries three orthogonal descriptors, each with a single
//! source of truth:
//! - **input kind** ([`JobInputKind`]) — what payload fed it; a
//!   projection of [`JobInput`]. Display / denormalisation only.
//! - **origin** ([`baybo_model::TriggerKind`], stored on `Job`) — the
//!   owning session's root trigger, recorded as-is at creation. Not
//!   asserted against the payload: a maintenance job runs inside a
//!   `User`-trigger session and records `origin = User` honestly.
//! - **shape** ([`JobShape`]) — whether the job runs a full agent-loop
//!   turn or a one-shot maintenance pass. Declared by the code path that
//!   runs the job, *not* derived from the payload: `/compact` and
//!   background compression are both `Maintenance` even though only the
//!   latter carries a `System` input.

use baybo_model::{BackgroundCompressionPayload, ContentBlock};
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
    System,
    Spawned,
    SubagentNotification,
}

/// Provenance for [`JobInput::System`] maintenance jobs.
///
/// This is persisted in `jobs.data`. Keep `#[serde(untagged)]` with
/// `Compression` first so legacy `{"up_to_ordinal":N}` payloads still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemJobPayload {
    /// Background transcript compression / summary pass.
    Compression(BackgroundCompressionPayload),
    /// One-off pass that titles a conversation from its first user question.
    TitleGeneration {},
}

/// Whether a job runs a full agent-loop turn or a one-shot maintenance
/// pass. A turn drives the LLM↔tool loop on behalf of its session; a
/// maintenance job (background compression, `/compact`) does focused
/// bookkeeping and never enters the loop. Read via `Job::is_turn()`.
///
/// Set by the code path that runs the job — `run()` is a `Turn`,
/// compression paths are `Maintenance` — rather than inferred from the
/// payload, which would mislabel a `/compact` (a `UserChat`-input
/// maintenance pass) as a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobShape {
    Turn,
    Maintenance,
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
    /// A maintenance task. The payload is provenance; the spawning code path
    /// decides behaviour.
    System {
        payload: SystemJobPayload,
    },
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
            JobInput::System { .. } => JobInputKind::System,
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
    /// Structured payload (for tool-direct cron jobs, system jobs whose
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

        let i = JobInput::System {
            payload: SystemJobPayload::Compression(BackgroundCompressionPayload {
                up_to_ordinal: 0,
            }),
        };
        assert_eq!(i.input_kind(), JobInputKind::System);
        let i = JobInput::System {
            payload: SystemJobPayload::TitleGeneration {},
        };
        assert_eq!(i.input_kind(), JobInputKind::System);

        let i = JobInput::Spawned {
            initial_prompt: vec![],
        };
        assert_eq!(i.input_kind(), JobInputKind::Spawned);

        let i = JobInput::SubagentNotification { content: vec![] };
        assert_eq!(i.input_kind(), JobInputKind::SubagentNotification);
    }

    #[test]
    fn input_round_trips_through_serde() {
        let i = JobInput::System {
            payload: SystemJobPayload::Compression(BackgroundCompressionPayload {
                up_to_ordinal: 0,
            }),
        };
        let s = serde_json::to_string(&i).unwrap();
        let back: JobInput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.input_kind(), i.input_kind());

        let i = JobInput::System {
            payload: SystemJobPayload::TitleGeneration {},
        };
        let s = serde_json::to_string(&i).unwrap();
        let back: JobInput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.input_kind(), i.input_kind());
    }

    #[test]
    fn legacy_system_payload_without_variant_tag_still_loads_as_compression() {
        let legacy = r#"{"kind":"system","payload":{"up_to_ordinal":7}}"#;
        let back: JobInput = serde_json::from_str(legacy).expect("legacy System payload loads");
        assert_eq!(back.input_kind(), JobInputKind::System);
        assert!(matches!(
            back,
            JobInput::System {
                payload: SystemJobPayload::Compression(BackgroundCompressionPayload {
                    up_to_ordinal: 7
                })
            }
        ));
        let title = r#"{"kind":"system","payload":{}}"#;
        let back: JobInput = serde_json::from_str(title).expect("title System payload loads");
        assert!(matches!(
            back,
            JobInput::System {
                payload: SystemJobPayload::TitleGeneration {}
            }
        ));
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
