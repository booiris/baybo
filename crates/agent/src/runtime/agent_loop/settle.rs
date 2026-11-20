//! The turn prelude for unapplied asks: put a result for every ask the
//! session left unapplied on the transcript, adjacent to the assistant row
//! that raised it, before anything else is appended. See "The turn prelude"
//! in `docs/modules/human-ask.md`.

use baybo_human_ask::SettledAsk;
use baybo_model::human_ask::{AskId, AskKind, AskPhase, AskResolution};
use baybo_model::{ContentBlock, Session};
use baybo_store::HumanAskRow;
use baybo_tools::{
    ApprovalDecision, ApprovalOutcome, ApprovalResolution, ToolError, ToolOutput, refusal_reason,
};
use baybo_trace::{LifecycleOutcome, StepKind};
use tracing::{debug, warn};

use super::{AgentLoop, BatchCall, BatchTurn, CallWork, DispatchedToolCall};
use crate::runtime::tool_executor::{ExecutedTool, PreExecuteGate};

impl AgentLoop {
    /// The row a Resume turn for the answered `ask_id` starts from, if it
    /// should run now.
    pub async fn resume_ready(&self, ask_id: &AskId) -> anyhow::Result<Option<HumanAskRow>> {
        Ok(self.asks.resume_ready(ask_id).await?)
    }

    /// Append a result for every unapplied ask of the session, in ledger
    /// order: the synthetic refusal for an abandoned ask, the answer for a
    /// resolved question, the denial for a denied approval, and for an
    /// approved call its re-execution under a fresh step (stamped applied
    /// first, so a crash mid-run never runs it twice). Returns how many
    /// results were appended. Media a re-executed tool produced for the user
    /// lands in `turn_attachments`; [`Self::settle_before_append`] has no final
    /// row to carry it and drops it.
    pub(super) async fn settle_unapplied_asks(
        &mut self,
        session: &mut Session,
        turn: BatchTurn<'_>,
        turn_attachments: &mut Vec<ContentBlock>,
    ) -> anyhow::Result<usize> {
        self.context_manager.ensure_seeded().await?;
        let settled = self.asks.settle_unapplied(&session.id).await?;
        if settled.is_empty() {
            return Ok(0);
        }
        debug!(session_id = %session.id, count = settled.len(), "settling unapplied asks");
        let mut calls: Vec<BatchCall> = Vec::new();
        for next in settled {
            let SettledAsk {
                ask_id,
                tool_call_id,
                kind,
                phase,
                accesses,
                resolution,
            } = next;
            if let Some(call) = calls.iter_mut().find(|call| call.id == tool_call_id) {
                call.asks.push(ask_id);
                continue;
            }
            let Some(dispatch) = self.persisted_tool_use(&tool_call_id) else {
                warn!(
                    session_id = %session.id,
                    tool_use_id = %tool_call_id,
                    "settling an ask whose tool_use is not in the window; nothing to answer"
                );
                self.asks.mark_applied(&ask_id).await?;
                continue;
            };
            if self.has_tool_result(&tool_call_id) {
                warn!(
                    session_id = %session.id,
                    tool_use_id = %tool_call_id,
                    "settling an ask whose call already has a result; appending nothing"
                );
                self.asks.mark_applied(&ask_id).await?;
                continue;
            }
            let work = settled_result(kind, phase, accesses, &resolution, &dispatch);
            calls.push(BatchCall {
                id: tool_call_id,
                dispatch,
                work,
                asks: vec![ask_id],
            });
        }
        if !calls
            .iter()
            .any(|call| matches!(call.work, CallWork::Run(_)))
        {
            return self
                .finish_calls(session, turn, calls, None, turn_attachments)
                .await;
        }
        crate::runtime::scope::with_step(
            turn.span_recorder,
            turn.turn_id,
            StepKind::LlmIteration,
            Some((
                &turn.cancellation.tokens.turn,
                baybo_turn::CancelReason::ParentCancelled,
            )),
            |step| async move {
                let count = self
                    .dispatch_calls(session, turn, &step, None, calls, turn_attachments)
                    .await?;
                Ok((LifecycleOutcome::Ok, count))
            },
        )
        .await
    }

    /// Whether the live window already answers `tool_use` `id` — a cancel
    /// fill that raced the ask's own write-ahead row, say.
    fn has_tool_result(&self, id: &str) -> bool {
        self.context_manager
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
            })
    }

    /// The `tool_use` block `id` on the live window, resolved through the
    /// `ToolInvoke` envelope exactly as the batch dispatched it.
    fn persisted_tool_use(&self, id: &str) -> Option<DispatchedToolCall> {
        self.context_manager
            .messages()
            .iter()
            .rev()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::ToolUse {
                    id: use_id,
                    name,
                    input,
                    signature,
                } if use_id == id => Some(baybo_llm::ToolCallInfo {
                    id: use_id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                    signature: signature.clone(),
                }),
                _ => None,
            })
            .map(|info| DispatchedToolCall::resolve(&info, &self.tool_registry))
    }
}

/// What `resolution` means for the call an ask of `kind` blocked. An
/// approval is read through [`ApprovalOutcome::from_ask`], the one reading
/// of a ledger close for a tool call, so a settled denial is worded exactly
/// as the live gate's.
fn settled_result(
    kind: AskKind,
    phase: AskPhase,
    accesses: Vec<baybo_tools::ResourceAccess>,
    resolution: &AskResolution,
    call: &DispatchedToolCall,
) -> CallWork {
    let question = |value| {
        CallWork::Ready(ExecutedTool {
            output: Ok(ToolOutput::Json(value)),
            approval: None,
            abandoned: false,
            handed_over: None,
        })
    };
    match (kind, resolution) {
        (AskKind::Question, AskResolution::Answered(answers)) => {
            question(baybo_human_ask::result::answered(answers))
        }
        (AskKind::Question, AskResolution::Declined) => {
            question(baybo_human_ask::result::declined())
        }
        (AskKind::Question, AskResolution::Cancelled) => {
            question(baybo_human_ask::result::cancelled())
        }
        (AskKind::Question, _) => question(baybo_human_ask::result::abandoned()),
        (AskKind::Approval, _) => {
            let outcome =
                ApprovalOutcome::from_ask(resolution).unwrap_or_else(ApprovalOutcome::abandoned);
            match outcome.decision {
                decision @ (ApprovalDecision::Approve | ApprovalDecision::ApproveAlways) => {
                    CallWork::Run(PreExecuteGate::PreDecided {
                        decision,
                        phase,
                        accesses,
                    })
                }
                ApprovalDecision::Deny => CallWork::Ready(denied(call, outcome.resolution)),
            }
        }
    }
}

fn denied(call: &DispatchedToolCall, resolution: ApprovalResolution) -> ExecutedTool {
    ExecutedTool {
        output: Err(anyhow::Error::new(ToolError::Denied {
            tool: call.name.clone(),
            reason: refusal_reason(resolution).to_string(),
        })),
        approval: Some(ApprovalDecision::Deny),
        abandoned: false,
        handed_over: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_denial_carries_the_deny_decision_and_the_typed_error() {
        let call = DispatchedToolCall {
            name: "Bash".into(),
            arguments: serde_json::json!({}),
            via_invoke: false,
        };
        let executed = denied(&call, ApprovalResolution::Abandoned);
        assert_eq!(executed.approval, Some(ApprovalDecision::Deny));
        let err = executed.output.expect_err("a denial");
        assert!(matches!(
            err.downcast_ref::<ToolError>(),
            Some(ToolError::Denied { reason, .. }) if reason == refusal_reason(ApprovalResolution::Abandoned)
        ));
    }
}
