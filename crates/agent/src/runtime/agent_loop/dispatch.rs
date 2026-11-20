use std::sync::Arc;

use baybo_channels::{AgentOutput, ToolStatus};
use baybo_model::human_ask::AskId;
use baybo_model::{ChatMessage, ContentBlock, Session, SpanId, TurnId};
use baybo_tools::{CancelOrigin, SessionNotifier, ToolConcurrency, TurnTokens};
use baybo_trace::{SpanRecorder, StepHandle};
use baybo_turn::TurnInputKind;
use futures::{StreamExt, stream::FuturesUnordered};
use parking_lot::Mutex;
use tokio::sync::{Semaphore, mpsc};

use super::{
    AgentLoop, DeltaTxNotifier, DispatchedToolCall, FinishedCall, ToolMedia, TurnCancellation,
};
use crate::runtime::cron_report_turn::CronReportTurn;
use crate::runtime::tool_executor::{ExecutedTool, PreExecuteGate, TurnBackground};

/// Upper bound on how many [`ToolConcurrency::Concurrent`] tool calls
/// run at once within a tool batch. A
/// [`ToolConcurrency::Exclusive`] call (any tool that mutates state)
/// acquires *all* of these permits, so it runs alone — it waits for
/// in-flight pool calls to drain and blocks any other pool call until it
/// returns. A [`ToolConcurrency::Independent`] call (`spawn_subagent`)
/// acquires no permit and self-bounds out-of-band, so the pool never
/// throttles subagent fan-out. Like the per-tool timeout ceiling, the
/// cap lives in code rather than `baybo.json`.
const MAX_CONCURRENT_TOOL_CALLS: usize = 10;

#[derive(Clone, Copy)]
pub(super) struct BatchTurn<'a> {
    pub(super) span_recorder: &'a Arc<SpanRecorder>,
    pub(super) turn_id: TurnId,
    pub(super) kind: TurnInputKind,
    pub(super) cancellation: &'a TurnCancellation,
    pub(super) delta_tx: Option<&'a mpsc::Sender<AgentOutput>>,
    pub(super) cron_report: Option<&'a CronReportTurn>,
}

pub(super) struct BatchCall {
    pub(super) id: String,
    pub(super) dispatch: DispatchedToolCall,
    pub(super) work: CallWork,
    /// Ledger order; the first decision determines this call's single result.
    pub(super) asks: Vec<AskId>,
}

pub(super) enum CallWork {
    Run(PreExecuteGate),
    Ready(ExecutedTool),
    Skip,
}

impl AgentLoop {
    pub(super) async fn dispatch_calls(
        &mut self,
        session: &mut Session,
        turn: BatchTurn<'_>,
        step: &StepHandle,
        llm_span: Option<SpanId>,
        mut calls: Vec<BatchCall>,
        turn_attachments: &mut Vec<ContentBlock>,
    ) -> anyhow::Result<usize> {
        for call in &calls {
            if llm_span.is_some() || matches!(call.work, CallWork::Run(_)) {
                let label = self
                    .tool_registry
                    .progress_label(&call.dispatch.name, &call.dispatch.arguments);
                self.emit_tool_started(
                    turn.delta_tx,
                    session,
                    call.id.clone(),
                    call.dispatch.name.clone(),
                    label,
                )
                .await;
            }
        }

        let approved = Arc::new(Mutex::new(session.state.approved_resources.clone()));
        let notifier = turn.delta_tx.map(|tx| {
            Arc::new(DeltaTxNotifier {
                tx: tx.clone(),
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
            }) as Arc<dyn SessionNotifier>
        });
        let limiter = &Semaphore::new(MAX_CONCURRENT_TOOL_CALLS);
        let agent_id = &session.state.agent_id_or_builtin();
        let background = TurnBackground::for_turn(session, turn.kind);
        let session_ref: &Session = session;
        let agent: &AgentLoop = self;
        let mut pending = FuturesUnordered::new();
        for BatchCall {
            id,
            dispatch,
            work,
            asks,
        } in &mut calls
        {
            let CallWork::Run(gate) = work else { continue };
            let gate = gate.clone();
            let approved = &approved;
            let notifier = notifier.clone();
            let permits = match self.tool_registry.concurrency(&dispatch.name) {
                ToolConcurrency::Concurrent => 1,
                ToolConcurrency::Exclusive => MAX_CONCURRENT_TOOL_CALLS as u32,
                ToolConcurrency::Independent => 0,
            };
            pending.push(async move {
                let _permit = if permits > 0 {
                    limiter.acquire_many(permits).await.ok()
                } else {
                    None
                };
                if let Some(ask) = asks.first() {
                    // A queued approval is still recoverable until execution owns it.
                    if turn.cancellation.origin().is_some() {
                        return Ok(());
                    }
                    match agent.asks.mark_applied(ask).await {
                        Ok(true) => {}
                        Ok(false) => {
                            *work = CallWork::Skip;
                            return Ok(());
                        }
                        Err(error) => {
                            *work = CallWork::Skip;
                            return Err(error);
                        }
                    }
                }
                let _running = agent.running_calls.start();
                tracing::debug!(tool = %dispatch.name, "executing tool call");
                let tokens = &turn.cancellation.tokens;
                let executed = agent
                    .tool_executor
                    .execute(
                        &dispatch.name,
                        dispatch.arguments.clone(),
                        &session_ref.id,
                        &session_ref.trigger,
                        agent_id,
                        &session_ref.user,
                        approved,
                        turn.span_recorder,
                        step,
                        llm_span,
                        id.clone(),
                        None,
                        Some(turn.turn_id),
                        TurnTokens {
                            turn: tokens.turn.child_token(),
                            release: tokens.release.clone(),
                        },
                        notifier,
                        Some(&agent.lite_client),
                        background,
                        turn.kind.is_attended(),
                        agent.read_tracker.clone(),
                        turn.cron_report.map(|report| report.silence.clone()),
                        dispatch.via_invoke,
                        gate,
                    )
                    .await;
                *work = CallWork::Ready(executed);
                Ok(())
            });
        }
        let mut claim_error = None;
        loop {
            tokio::select! {
                biased;
                next = pending.next() => match next {
                    Some(Ok(())) => {}
                    Some(Err(error)) => { claim_error.get_or_insert(error); }
                    None => break,
                },
                _ = turn.cancellation.cancelled() => break,
            }
        }
        drop(pending);
        let origin = turn.cancellation.origin();
        let interrupted = calls
            .iter()
            .any(|call| matches!(call.work, CallWork::Run(_)));
        session.state.approved_resources = approved.lock().clone();
        let count = self
            .finish_calls(session, turn, calls, origin, turn_attachments)
            .await?;
        if let Some(error) = claim_error {
            return Err(error.into());
        }
        anyhow::ensure!(!interrupted, "turn cancelled during tool execution");
        Ok(count)
    }

    pub(super) async fn finish_calls(
        &mut self,
        session: &mut Session,
        turn: BatchTurn<'_>,
        calls: Vec<BatchCall>,
        origin: Option<CancelOrigin>,
        turn_attachments: &mut Vec<ContentBlock>,
    ) -> anyhow::Result<usize> {
        let mut appended = 0;
        let mut llm_visible_images = Vec::new();
        for BatchCall {
            id,
            dispatch,
            work,
            asks,
        } in calls
        {
            let executed = match work {
                CallWork::Ready(executed) => Some(executed),
                // Dropping the pending futures leaves unfinished slots in Run.
                CallWork::Run(_) => None,
                CallWork::Skip => continue,
            };
            if executed.as_ref().is_none_or(|e| e.abandoned)
                && !self.claim_interrupted_call(session, &id, origin).await
            {
                continue;
            }
            if let Some(executed) = executed {
                self.append_executed_tool(
                    session,
                    turn.delta_tx,
                    turn.turn_id,
                    FinishedCall {
                        tool_use_id: &id,
                        dispatch: &dispatch,
                        executed,
                    },
                    ToolMedia {
                        turn_attachments,
                        llm_visible_images: &mut llm_visible_images,
                    },
                    turn.cron_report,
                )
                .await?;
            } else {
                self.emit_tool_completed(
                    turn.delta_tx,
                    session,
                    id.clone(),
                    (ToolStatus::Error, "cancelled".to_string()),
                    None,
                    None,
                )
                .await;
                let wrapped = baybo_model::wrap_tool_output(
                    &dispatch.name,
                    baybo_context::prompts::cancelled_turn::TOOL_RESULT_BODY,
                    &[],
                );
                self.context_manager
                    .append(&ChatMessage::tool_result_with_meta(id, wrapped, None))
                    .await?;
            }
            appended += 1;
            for ask in asks {
                self.asks.mark_applied(&ask).await?;
            }
        }
        self.append_llm_visible_images(llm_visible_images).await?;
        Ok(appended)
    }
}
