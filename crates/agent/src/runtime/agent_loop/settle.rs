//! The turn prelude for unapplied asks: put a result for every ask the
//! session left unapplied on the transcript, adjacent to the assistant row
//! that raised it, before anything else is appended. See "The turn prelude"
//! in `docs/modules/human-ask.md`.

use std::sync::Arc;

use baybo_human_ask::Settled;
use baybo_model::human_ask::{AskId, AskKind, AskResolution};
use baybo_model::{ContentBlock, Session, TurnId};
use baybo_store::HumanAskRow;
use baybo_tools::{
    ApprovalDecision, ApprovalResolution, ToolError, ToolOutput, TurnTokens, refusal_reason,
};
use baybo_trace::{LifecycleOutcome, SpanRecorder, StepKind};
use baybo_turn::{TurnInput, TurnInputKind, TurnLifecycle, TurnOutput};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{AgentLoop, DispatchedToolCall};
use crate::runtime::tool_executor::{ExecutedTool, PreExecuteGate};
use baybo_channels::AgentOutput;

/// What follows the settlement inside its Resume turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleMode {
    /// A door's prelude: the turn ends after the appends, and the door's own
    /// turn follows.
    SettleOnly,
    /// The chat Resume door: the same turn goes on to the LLM iterations.
    Continue,
}

/// The persisted `tool_use` a settlement answers, as the batch would have
/// dispatched it.
struct SettledCall {
    id: String,
    dispatch: DispatchedToolCall,
}

impl AgentLoop {
    /// The session's asks not yet applied to its transcript.
    pub async fn unapplied_asks(
        &self,
        session: &baybo_model::SessionId,
    ) -> anyhow::Result<Vec<HumanAskRow>> {
        self.asks.unapplied(session).await
    }

    pub async fn ask(&self, ask_id: &AskId) -> anyhow::Result<Option<HumanAskRow>> {
        self.asks.get(ask_id).await
    }

    /// Run `turn_input` (a `Resume`) as a turn whose whole body is the
    /// settlement: [`SettleMode::SettleOnly`].
    pub async fn run_settlement_turn(
        &mut self,
        session: &mut Session,
        turn_input: TurnInput,
        turn_lifecycle: &Arc<TurnLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        tokens: TurnTokens,
    ) -> anyhow::Result<usize> {
        self.refresh_active_llm();
        let spec = crate::runtime::scope::TurnSpec {
            session_id: session.id.clone(),
            origin: session.trigger.kind(),
            input: turn_input,
            parent_turn_id: None,
        };
        crate::runtime::scope::with_turn(
            turn_lifecycle,
            tokens.turn.clone(),
            spec,
            |turn_id| async move {
                let mut attachments = Vec::new();
                let settled = self
                    .settle_unapplied_asks(
                        session,
                        span_recorder,
                        turn_id,
                        &tokens,
                        delta_tx.as_ref(),
                        SettleMode::SettleOnly,
                        &mut attachments,
                    )
                    .await?;
                Ok((
                    TurnOutput::Structured {
                        value: serde_json::json!({ "settled_asks": settled }),
                    },
                    settled,
                ))
            },
        )
        .await
    }

    /// Append a result for every unapplied ask of the session, in ledger
    /// order: the synthetic refusal for an abandoned ask, the answer for a
    /// resolved question, the denial for a denied approval, and for an
    /// approved call its re-execution under a fresh step (stamped applied
    /// first, so a crash mid-run never runs it twice). Returns how many
    /// results were appended. Media a re-executed tool produced for the user
    /// lands in `turn_attachments`; under [`SettleMode::SettleOnly`] there is
    /// no final row to carry it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn settle_unapplied_asks(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        tokens: &TurnTokens,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        mode: SettleMode,
        turn_attachments: &mut Vec<ContentBlock>,
    ) -> anyhow::Result<usize> {
        self.context_manager.ensure_seeded().await?;
        let settled = self.asks.settle_unapplied(&session.id).await?;
        if settled.is_empty() {
            return Ok(0);
        }
        debug!(session_id = %session.id, count = settled.len(), ?mode, "settling unapplied asks");
        let mut llm_visible_images = Vec::new();
        let mut appended = 0;
        for next in settled {
            let (ask_id, tool_call_id, resolution, kind) = match next {
                Settled::Abandoned { tool_call_id, kind } => {
                    (None, tool_call_id, AskResolution::Abandoned, kind)
                }
                Settled::Apply {
                    ask_id,
                    tool_call_id,
                    kind,
                    resolution,
                } => (Some(ask_id), tool_call_id, resolution, kind),
            };
            let Some(call) = self.persisted_tool_use(&tool_call_id) else {
                warn!(
                    session_id = %session.id,
                    tool_use_id = %tool_call_id,
                    "settling an ask whose tool_use is not in the window; nothing to answer"
                );
                if let Some(ask_id) = &ask_id {
                    self.asks.mark_applied(ask_id).await?;
                }
                continue;
            };
            let executed = match (&resolution, kind) {
                (
                    AskResolution::Approval(
                        decision @ (ApprovalDecision::Approve | ApprovalDecision::ApproveAlways),
                    ),
                    _,
                ) => {
                    let Some(ask_id) = &ask_id else { continue };
                    if !self.asks.mark_applied(ask_id).await? {
                        continue;
                    }
                    let executed = self
                        .reexecute(
                            session,
                            span_recorder,
                            turn_id,
                            tokens,
                            delta_tx,
                            &call,
                            *decision,
                        )
                        .await?;
                    self.append_settled(
                        session,
                        delta_tx,
                        turn_id,
                        &call,
                        executed,
                        turn_attachments,
                        &mut llm_visible_images,
                    )
                    .await?;
                    appended += 1;
                    continue;
                }
                (AskResolution::Answered(answers), _) => ExecutedTool {
                    output: Ok(ToolOutput::Json(baybo_human_ask::result::answered(answers))),
                    approval: None,
                    abandoned: false,
                },
                (AskResolution::Declined, _) => ExecutedTool {
                    output: Ok(ToolOutput::Json(baybo_human_ask::result::declined())),
                    approval: None,
                    abandoned: false,
                },
                (AskResolution::Abandoned, AskKind::Question) => ExecutedTool {
                    output: Ok(ToolOutput::Json(baybo_human_ask::result::abandoned())),
                    approval: None,
                    abandoned: false,
                },
                (AskResolution::Approval(ApprovalDecision::Deny), _) => {
                    denied(&call, ApprovalResolution::Answered)
                }
                (AskResolution::Abandoned, AskKind::Approval) => {
                    denied(&call, ApprovalResolution::Abandoned)
                }
                (AskResolution::PolicyDenied, _) => denied(&call, ApprovalResolution::Policy),
            };
            self.append_settled(
                session,
                delta_tx,
                turn_id,
                &call,
                executed,
                turn_attachments,
                &mut llm_visible_images,
            )
            .await?;
            appended += 1;
            if let Some(ask_id) = &ask_id {
                self.asks.mark_applied(ask_id).await?;
            }
        }
        self.append_llm_visible_images(llm_visible_images).await?;
        Ok(appended)
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_settled(
        &mut self,
        session: &mut Session,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        turn_id: TurnId,
        call: &SettledCall,
        executed: ExecutedTool,
        turn_attachments: &mut Vec<ContentBlock>,
        llm_visible_images: &mut Vec<ContentBlock>,
    ) -> anyhow::Result<()> {
        self.append_executed_tool(
            session,
            delta_tx,
            turn_id,
            &call.id,
            &call.dispatch,
            executed,
            turn_attachments,
            llm_visible_images,
        )
        .await
    }

    /// The `tool_use` block `id` on the live window, resolved through the
    /// `ToolInvoke` envelope exactly as the batch dispatched it.
    fn persisted_tool_use(&self, id: &str) -> Option<SettledCall> {
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
            .map(|info| SettledCall {
                id: info.id.clone(),
                dispatch: DispatchedToolCall::resolve(&info, &self.tool_registry),
            })
    }

    /// Run an approved call again with the human's decision standing in for
    /// the gate, under a fresh step of the current turn.
    #[allow(clippy::too_many_arguments)]
    async fn reexecute(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        tokens: &TurnTokens,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        call: &SettledCall,
        decision: ApprovalDecision,
    ) -> anyhow::Result<ExecutedTool> {
        let label = self
            .tool_registry
            .progress_label(&call.dispatch.name, &call.dispatch.arguments);
        self.emit_tool_started(
            delta_tx,
            session,
            call.id.clone(),
            call.dispatch.name.clone(),
            label,
        )
        .await;
        let notifier: Option<Arc<dyn baybo_tools::SessionNotifier>> = delta_tx.map(|tx| {
            Arc::new(super::DeltaTxNotifier {
                tx: tx.clone(),
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
            }) as Arc<dyn baybo_tools::SessionNotifier>
        });
        let approved = Arc::new(parking_lot::Mutex::new(
            session.state.approved_resources.clone(),
        ));
        self.read_tracker.begin_response();
        let executor = Arc::clone(&self.tool_executor);
        let bind_source = Arc::clone(&self.lite_client);
        let read_tracker = self.read_tracker.clone();
        let agent_id = session.state.agent_id_or_builtin();
        let background_eligible =
            crate::runtime::background_jobs::background_eligible(session, TurnInputKind::Resume);
        let call_tokens = TurnTokens {
            turn: tokens.turn.child_token(),
            actor: tokens.actor.clone(),
        };
        let approved_for_call = Arc::clone(&approved);
        let session_ref: &Session = session;
        let executed = crate::runtime::scope::with_step(
            span_recorder.as_ref(),
            turn_id,
            StepKind::LlmIteration,
            Some((&tokens.turn, baybo_turn::CancelReason::ParentCancelled)),
            |step| async move {
                let executed = executor
                    .execute(
                        &call.dispatch.name,
                        call.dispatch.arguments.clone(),
                        &session_ref.id,
                        &session_ref.trigger,
                        &agent_id,
                        &session_ref.user,
                        &approved_for_call,
                        span_recorder,
                        &step,
                        None,
                        call.id.clone(),
                        None,
                        Some(turn_id),
                        call_tokens,
                        notifier,
                        Some(&bind_source),
                        background_eligible,
                        TurnInputKind::Resume.is_attended(),
                        read_tracker,
                        None,
                        call.dispatch.via_invoke,
                        PreExecuteGate::PreDecided(decision),
                    )
                    .await;
                Ok((LifecycleOutcome::Ok, executed))
            },
        )
        .await?;
        session.state.approved_resources = approved.lock().clone();
        Ok(executed)
    }
}

/// The denial an approval refused by `resolution` reads as, in the same shape
/// the executor's gate produces live.
fn denied(call: &SettledCall, resolution: ApprovalResolution) -> ExecutedTool {
    ExecutedTool {
        output: Err(anyhow::Error::new(ToolError::Denied {
            tool: call.dispatch.name.clone(),
            reason: refusal_reason(resolution).to_string(),
        })),
        approval: Some(ApprovalDecision::Deny),
        abandoned: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_denial_carries_the_deny_decision_and_the_typed_error() {
        let call = SettledCall {
            id: "t1".into(),
            dispatch: DispatchedToolCall {
                name: "Bash".into(),
                arguments: serde_json::json!({}),
                via_invoke: false,
            },
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
