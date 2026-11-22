use std::sync::Arc;

use baybo_channels::AgentOutput;
use baybo_model::{ChatMessage, ContentBlock, Session};
use baybo_session::SessionMessageAppendOutcome;
use baybo_tools::TurnTokens;
use baybo_trace::{CompressionTrigger, SpanRecorder};
use baybo_turn::{TurnInput, TurnInputKind, TurnLifecycle, TurnOutput};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::{AgentLoop, BatchTurn, CompactionNotice, TurnCancellation, compaction_failed_text};
use crate::runtime::scope::TurnSpec;

/// Exclusive access to a loop's transcript after its outstanding asks have been settled.
pub(crate) struct SettledTranscript<'a> {
    agent_loop: &'a mut AgentLoop,
}

impl AgentLoop {
    /// A door's prelude: settle the session's unapplied asks before the door
    /// appends anything, so each result lands adjacent to the row that
    /// raised it. Opens no turn when there is nothing to settle; otherwise
    /// the settlement is an `AskSettlement` turn of its own, which ends after the
    /// appends, and the door's turn follows. (The chat Resume door instead
    /// settles inside `run_inner` and goes on to the LLM iterations.)
    pub(crate) async fn settle_before_append(
        &mut self,
        session: &mut Session,
        turn_lifecycle: &Arc<TurnLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        tokens: TurnTokens,
    ) -> anyhow::Result<SettledTranscript<'_>> {
        if self
            .asks
            .unapplied_for_session(&session.id)
            .await?
            .is_empty()
        {
            return Ok(SettledTranscript { agent_loop: self });
        }
        self.refresh_active_llm();
        let spec = TurnSpec {
            session_id: session.id.clone(),
            origin: session.trigger.kind(),
            input: TurnInput::AskSettlement,
            parent_turn_id: None,
        };
        let cancellation = &TurnCancellation {
            tokens,
            observed: Default::default(),
        };
        crate::runtime::scope::with_turn_reporting_cancel(
            turn_lifecycle,
            cancellation.tokens.turn.clone(),
            spec,
            |turn_id| async move {
                let mut attachments = Vec::new();
                let settled = self
                    .settle_unapplied_asks(
                        session,
                        BatchTurn {
                            span_recorder,
                            turn_id,
                            kind: TurnInputKind::AskSettlement,
                            cancellation,
                            cron_report: None,
                            delta_tx: delta_tx.as_ref(),
                        },
                        &mut attachments,
                    )
                    .await?;
                Ok((
                    TurnOutput::Structured {
                        value: serde_json::json!({ "settled_asks": settled }),
                    },
                    SettledTranscript { agent_loop: self },
                ))
            },
            |error| cancellation.classify(error),
        )
        .await
    }
}

impl SettledTranscript<'_> {
    /// Append a user-authored message to this session's transcript — both the
    /// in-memory context and the persisted `session_messages` log — ahead
    /// of the turn that runs next. Lets the actor coalesce a burst of user
    /// messages into one turn while keeping each as its own transcript row;
    /// context's `messages_for_llm` collapses the consecutive rows for the
    /// provider call.
    pub(crate) async fn append_user_message(
        &mut self,
        content: Vec<ContentBlock>,
        platform_msg_id: impl Into<String>,
    ) -> anyhow::Result<()> {
        let msg = ChatMessage::user(content).with_platform_msg_id(platform_msg_id);
        self.append(msg, None).await?;
        Ok(())
    }

    /// Append the card a run was handed. `files` are the card's own
    /// attachments, already resolved into blocks: they follow the framed
    /// prose rather than being wrapped by it, so the framing's contract —
    /// that the instruction is the tail of the *text* — still holds with a
    /// picture in the message.
    pub(crate) async fn append_issue_brief(
        &mut self,
        number: i64,
        checkout: &str,
        brief: &str,
        files: &[baybo_model::MediaBlock],
    ) -> anyhow::Result<()> {
        let framed = baybo_context::prompts::issue::frame_issue_brief(number, checkout, brief);
        let msg = ChatMessage::issue_brief(baybo_model::prose_with_media(framed, files));
        self.append(msg, None).await?;
        Ok(())
    }

    /// Append a cron fire's framed prompt as a persisted `Cron`-source row
    /// ahead of the turn. The framing ([`baybo_context::prompts::cron`]) makes
    /// the model treat the fire as a task to perform now rather than a live
    /// user message; `MessageSource::Cron` lets the operator inbox find the
    /// row. Seeds the system prompt first so a fresh cron session never lands
    /// the fire ahead of `messages[0]`.
    pub(crate) async fn append_cron_fire(
        &mut self,
        job_id: &str,
        prompt: &str,
        context: Option<&str>,
    ) -> anyhow::Result<()> {
        let framed =
            baybo_context::prompts::cron::frame_cron_prompt_with_context(job_id, prompt, context);
        let msg = ChatMessage::cron_fire(vec![ContentBlock::Text(framed)]);
        self.append(msg, None).await?;
        Ok(())
    }

    /// Append a one-shot cron fire's result to **this** (the scheduling)
    /// conversation as a persisted `Role::Assistant` row. A one-shot origin
    /// delivery supplies `source_event_id` so a crash replay returns the
    /// existing row instead of appending another copy; recurring fires pass
    /// `None` because each fire owns a fresh conversation.
    ///
    /// No inference runs: `content` is the fire's own reply, already framed
    /// with a scheduled-task header ([`baybo_context::prompts::cron`]). The
    /// row is stamped `MessageSource::CronNotification`, so chat surfaces can
    /// badge it and the next real turn reads it back as something the
    /// assistant already reported. Seeds the system prompt first — the
    /// notification can be the first thing an otherwise-cold session appends.
    pub(crate) async fn append_cron_notification(
        &mut self,
        content: Vec<ContentBlock>,
        source_event_id: Option<&str>,
    ) -> anyhow::Result<SessionMessageAppendOutcome> {
        self.append(ChatMessage::cron_notification(content), source_event_id)
            .await
    }

    /// Append the crash-replayable hidden prompt that carries a background
    /// batch into its analysis turn. Existing rows are returned without
    /// duplicating the live context window.
    pub(crate) async fn append_background_notification_prompt_once(
        &mut self,
        content: Vec<ContentBlock>,
        source_event_id: &str,
    ) -> anyhow::Result<SessionMessageAppendOutcome> {
        self.append(ChatMessage::agent_context(content), Some(source_event_id))
            .await
    }

    async fn append(
        &mut self,
        message: ChatMessage,
        source_event_id: Option<&str>,
    ) -> anyhow::Result<SessionMessageAppendOutcome> {
        let context = &mut self.agent_loop.context_manager;
        context.ensure_seeded().await?;
        Ok(match source_event_id {
            Some(id) => context.append_idempotent(id, &message).await?,
            None => SessionMessageAppendOutcome::Inserted {
                ordinal: context.append(&message).await?,
            },
        })
    }

    /// Run an on-demand compression pass and return the notice for the caller
    /// to ship as an `AgentEvent::Notice`.
    /// The variants of `CompressionOutcome` map to specific user-facing
    /// messages so the caller (typically a `/compact` notice) can
    /// distinguish "strategy declined", "no savings", a failed summarizer
    /// call, and a real compress — instead of one generic "nothing to
    /// compress" line.
    /// A fresh turn is minted so the compression step + LLM span land
    /// on a real lifecycle.
    pub(crate) async fn compact_now(
        self,
        session: &Session,
        turn_lifecycle: &Arc<TurnLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<CompactionNotice> {
        // `/compact` is a user-typed command, but it is not a chat turn: it
        // runs `force_compress` directly and emits a notice, not an assistant
        // reply.
        let turn_input = TurnInput::Compact;
        let spec = TurnSpec {
            session_id: session.id.clone(),
            origin: session.trigger.kind(),
            input: turn_input,
            parent_turn_id: None,
        };

        crate::runtime::scope::with_turn(
            turn_lifecycle,
            cancel_token.clone(),
            spec,
            |turn_id| async move {
                let runner = self.agent_loop.build_compression_runner(
                    session,
                    span_recorder,
                    turn_id,
                    &cancel_token,
                    CompressionTrigger::Forced,
                );
                let model_id = runner.model_info.id.clone();
                let tool_defs = self.agent_loop.session_tool_defs(session);
                let outcome = self.agent_loop
                    .context_manager
                    .force_compress(&model_id, tool_defs, |req, marker| async move {
                        runner.run(req, marker).await
                    })
                    .await?;
                let notice = match outcome {
                    baybo_context::CompressionOutcome::Compressed => CompactionNotice::info(
                        "Context compressed.",
                    ),
                    baybo_context::CompressionOutcome::BelowThreshold => CompactionNotice::info(
                        "Context already under the compression threshold; skipped.",
                    ),
                    baybo_context::CompressionOutcome::StrategyDeclined => CompactionNotice::info(
                        "Compression strategy declined: nothing to summarize (conversation too short).",
                    ),
                    baybo_context::CompressionOutcome::NoSavings => CompactionNotice::info(
                        "Compression ran but produced no savings; kept the original.",
                    ),
                    baybo_context::CompressionOutcome::Cancelled => CompactionNotice::info(
                        "Compaction cancelled; the conversation is unchanged.",
                    ),
                    // The one arm the user has to act on — retype `/compact`,
                    // or let the threshold path retry — so it is the one arm
                    // that does not come back as `Info`.
                    baybo_context::CompressionOutcome::Failed { reason } => {
                        warn!(
                            session_id = %session.id,
                            reason = %reason,
                            "/compact failed; transcript left unchanged"
                        );
                        CompactionNotice::warn(&compaction_failed_text(&reason))
                    }
                };
                let output = TurnOutput::Message {
                    content: vec![ContentBlock::Text(notice.text.clone())],
                    // `/compact` is not a user-chat turn — never pushed.
                    ordinal: None,
                };
                Ok((output, notice))
            },
        )
        .await
    }
}
