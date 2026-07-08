//! Fold a session's buffered in-flight progress events into work-block steps.
//!
//! The single source of truth for turning a [`Channel::in_flight_events`]
//! snapshot (the reasoning / answer / tool events of the turn currently
//! streaming) into a compact, coalesced step list. Two consumers share it:
//!
//! * the [`Frame::SubscribeState`] bundle (`channel::route`), whose
//!   `work_steps` half ships [`WireWorkStep`]s to a client that
//!   subscribed mid-turn, and
//! * the REST transcript reconstruction (`api::admin::chat`), which folds
//!   the same steps into the newest page's trailing work block (deriving
//!   its `ChatWorkStep` via `From`).
//!
//! [`Channel::in_flight_events`]: baybo_channels::Channel::in_flight_events
//! [`Frame::SubscribeState`]: baybo_channels::wire::Frame::SubscribeState

use baybo_channels::wire::WireWorkStep;
use baybo_channels::{AgentEvent, SessionEvent, ToolStatus};
use std::collections::HashMap;

fn tool_status_str(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "ok",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
    }
}

/// Fold buffered in-flight events into ordered work steps: reasoning / prose /
/// progress-narration bodies pass through, and a `ToolCompleted` resolves the
/// `status` / `summary` of the matching earlier `ToolStarted` step (paired by
/// `call_id`; an orphan completion with no matching start is dropped, matching
/// the live client's own tolerance). Empty-text bodies are skipped. The buffer
/// already coalesces consecutive same-kind text deltas, so a run reads as one
/// step here.
pub(crate) fn in_flight_wire_steps(events: Vec<SessionEvent>) -> Vec<WireWorkStep> {
    let mut steps: Vec<WireWorkStep> = Vec::new();
    let mut pending_tools: HashMap<String, usize> = HashMap::new();
    for ev in events {
        let SessionEvent::Agent(out) = ev else {
            continue;
        };
        match out.event {
            AgentEvent::Reasoning(text) if !text.trim().is_empty() => {
                steps.push(WireWorkStep::reasoning(text));
            }
            AgentEvent::AnswerDelta(text) if !text.trim().is_empty() => {
                steps.push(WireWorkStep::prose(text));
            }
            AgentEvent::Progress(text) if !text.trim().is_empty() => {
                steps.push(WireWorkStep::status(text));
            }
            AgentEvent::ToolStarted {
                call_id,
                tool,
                label,
            } => {
                pending_tools.insert(call_id.clone(), steps.len());
                steps.push(WireWorkStep::tool(Some(call_id), Some(tool), label));
            }
            AgentEvent::ToolCompleted {
                call_id,
                status,
                summary,
            } => {
                if let Some(&idx) = pending_tools.get(&call_id)
                    && let Some(step) = steps.get_mut(idx)
                {
                    step.status = Some(tool_status_str(status).to_owned());
                    step.summary = Some(summary);
                }
            }
            _ => {}
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_channels::AgentOutput;
    use baybo_channels::wire::WireWorkStepKind;
    use baybo_model::{ChannelType, SessionId};

    fn ev(event: AgentEvent) -> SessionEvent {
        SessionEvent::Agent(AgentOutput {
            session_id: SessionId::from("s"),
            user_id: "u".into(),
            channel: ChannelType::http(),
            event,
        })
    }

    #[test]
    fn folds_reasoning_tool_and_prose_retaining_call_id() {
        let steps = in_flight_wire_steps(vec![
            ev(AgentEvent::Reasoning("weighing it".into())),
            ev(AgentEvent::ToolStarted {
                call_id: "c1".into(),
                tool: "Bash".into(),
                label: Some("ls".into()),
            }),
            ev(AgentEvent::ToolCompleted {
                call_id: "c1".into(),
                status: ToolStatus::Ok,
                summary: "exit 0".into(),
            }),
            ev(AgentEvent::AnswerDelta("the answer".into())),
        ]);

        assert_eq!(steps.len(), 3, "reasoning + tool + prose: {steps:?}");
        assert_eq!(steps[0].kind, WireWorkStepKind::Reasoning);
        assert_eq!(steps[0].text, "weighing it");
        assert_eq!(steps[1].kind, WireWorkStepKind::Tool);
        assert_eq!(
            steps[1].call_id.as_deref(),
            Some("c1"),
            "call_id kept for live pairing"
        );
        assert_eq!(steps[1].tool.as_deref(), Some("Bash"));
        assert_eq!(steps[1].label.as_deref(), Some("ls"));
        assert_eq!(steps[1].status.as_deref(), Some("ok"));
        assert_eq!(steps[1].summary.as_deref(), Some("exit 0"));
        assert_eq!(steps[2].kind, WireWorkStepKind::Prose);
        assert_eq!(steps[2].text, "the answer");
    }

    #[test]
    fn progress_narration_folds_into_a_status_step() {
        let steps = in_flight_wire_steps(vec![
            ev(AgentEvent::Reasoning("thinking".into())),
            ev(AgentEvent::Progress("Reading the config".into())),
            // Blank progress is skipped like blank reasoning.
            ev(AgentEvent::Progress("  ".into())),
        ]);

        assert_eq!(steps.len(), 2, "reasoning + one status: {steps:?}");
        assert_eq!(steps[1].kind, WireWorkStepKind::Status);
        assert_eq!(steps[1].text, "Reading the config");
        assert!(steps[1].call_id.is_none() && steps[1].tool.is_none());
    }

    #[test]
    fn running_tool_has_no_status_and_orphan_completion_is_dropped() {
        let steps = in_flight_wire_steps(vec![
            ev(AgentEvent::ToolStarted {
                call_id: "c1".into(),
                tool: "Bash".into(),
                label: None,
            }),
            // A completion for a call we never saw start — dropped, not appended.
            ev(AgentEvent::ToolCompleted {
                call_id: "ghost".into(),
                status: ToolStatus::Error,
                summary: "boom".into(),
            }),
            // Blank reasoning is skipped.
            ev(AgentEvent::Reasoning("   ".into())),
        ]);

        assert_eq!(steps.len(), 1, "only the started tool survives: {steps:?}");
        assert_eq!(steps[0].kind, WireWorkStepKind::Tool);
        assert_eq!(steps[0].status, None, "unfinished tool has no status");
        assert_eq!(steps[0].summary, None);
    }
}
