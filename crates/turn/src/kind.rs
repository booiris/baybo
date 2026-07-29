//! `TurnInputKind`, `TurnInput`, `TurnOutput`.
//!
//! A turn carries three orthogonal descriptors, each with a single
//! source of truth:
//! - **input kind** ([`TurnInputKind`]) — what payload fed it; a
//!   projection of [`TurnInput`]. Display / denormalisation only.
//! - **origin** ([`baybo_model::TriggerKind`], stored on `Turn`) — the
//!   owning session's root trigger, recorded as-is at creation. Not
//!   asserted against the payload: a maintenance turn runs inside a
//!   `User`-trigger session and records `origin = User` honestly.

use baybo_model::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What payload fed this turn — a projection of [`TurnInput`]. Used for
/// display and the denormalised `turns.kind` column only; behaviour
/// branches on [`TurnInput`] itself, never on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputKind {
    UserChat,
    Cron,
    CronNotification,
    Compact,
    Spawned,
    SubagentNotification,
}

/// What initially fed this turn.
///
/// `Cron::action_payload` is an opaque trace blob written by the cron
/// router (currently `{cron_job_id, prompt}`). Kept as `Value` here so
/// `baybo-turn` does not depend on `baybo-cron`, which would invert the
/// layer order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnInput {
    UserChat {
        content: Vec<ContentBlock>,
    },
    Cron {
        action_payload: Value,
    },
    /// Delivery of a one-shot cron fire's result into the conversation that
    /// scheduled it. Runs **no inference** — the actor appends the framed
    /// result and dispatches it — but opens a real turn so the delivery has
    /// lifecycle, and so its `Completed { reply_ordinal }` edge drives the
    /// push dispatcher off the row it just appended (the same durable-row
    /// contract as a user turn). `content` is the notification as persisted.
    CronNotification {
        content: Vec<ContentBlock>,
    },
    /// User-requested foreground compaction (`/compact`). It opens a real turn
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

impl TurnInput {
    pub fn input_kind(&self) -> TurnInputKind {
        match self {
            TurnInput::UserChat { .. } => TurnInputKind::UserChat,
            TurnInput::Cron { .. } => TurnInputKind::Cron,
            TurnInput::CronNotification { .. } => TurnInputKind::CronNotification,
            TurnInput::Compact => TurnInputKind::Compact,
            TurnInput::Spawned { .. } => TurnInputKind::Spawned,
            TurnInput::SubagentNotification { .. } => TurnInputKind::SubagentNotification,
        }
    }
}

/// What the turn produced as its contractual output (the value that
/// acceptance / review workflows judge against).
///
/// Progress messages emitted **mid-turn** do not live here — they are in
/// the trace tree, indexed by `Turn.emitted_span_ids`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutput {
    /// The final user-facing reply (for chat-style turns). `ordinal` is the
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
    /// Structured payload (for tool-direct cron jobs, maintenance turns whose
    /// product is structured data, etc.).
    Structured { value: Value },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_kind_matches_variant() {
        let i = TurnInput::UserChat { content: vec![] };
        assert_eq!(i.input_kind(), TurnInputKind::UserChat);

        let i = TurnInput::Cron {
            action_payload: serde_json::json!({}),
        };
        assert_eq!(i.input_kind(), TurnInputKind::Cron);

        let i = TurnInput::Compact;
        assert_eq!(i.input_kind(), TurnInputKind::Compact);

        let i = TurnInput::Spawned {
            initial_prompt: vec![],
        };
        assert_eq!(i.input_kind(), TurnInputKind::Spawned);

        let i = TurnInput::SubagentNotification { content: vec![] };
        assert_eq!(i.input_kind(), TurnInputKind::SubagentNotification);
    }

    #[test]
    fn input_round_trips_through_serde() {
        for input in [
            TurnInput::UserChat { content: vec![] },
            TurnInput::Cron {
                action_payload: serde_json::json!({"cron_job_id": "c1", "prompt": "run"}),
            },
            TurnInput::Compact,
            TurnInput::Spawned {
                initial_prompt: vec![ContentBlock::Text("task".into())],
            },
            TurnInput::SubagentNotification { content: vec![] },
        ] {
            let s = serde_json::to_string(&input).unwrap();
            let back: TurnInput = serde_json::from_str(&s).unwrap();
            assert_eq!(back.input_kind(), input.input_kind());
        }
    }

    #[test]
    fn output_round_trips_through_serde() {
        let o = TurnOutput::Message {
            content: vec![],
            ordinal: None,
        };
        let s = serde_json::to_string(&o).unwrap();
        let _: TurnOutput = serde_json::from_str(&s).unwrap();
    }
}
