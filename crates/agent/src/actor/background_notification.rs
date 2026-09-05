//! Durable delivery pipeline for terminal background-turn results.
//!
//! The pipeline has two stages that may coexist:
//!
//! 1. `buffered_results` / `groups` collect results that have not yet been
//!    committed to the transcript.
//! 2. `active_delivery` tracks one batch whose hidden prompt is durable but
//!    whose analysed report has not settled.
//!
//! The actor owns scheduling and persistence; `baybo-model` owns the durable
//! data shape. Keeping the orchestration here makes the buffer-to-ledger
//! durability boundary explicit and keeps the main actor loop about routing.

use baybo_channels::{AgentEvent, AgentOutput, NoticeLevel};
use baybo_model::{
    BackgroundNotificationDelivery, ContentBlock, ControlEventKind, PendingBackgroundResult,
};
use baybo_turn::TurnInput;
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};

use super::{AgentActor, AgentMessage, is_blank_reply, is_turn_cancelled, mailbox};

/// Maximum terminal results waiting to be committed to a notification prompt.
/// Dropped results remain recoverable from their child transcript or command
/// output file.
const MAX_BUFFERED_RESULTS: usize = 64;

/// How long a sealed group waits for every member before releasing a partial
/// batch and allowing stragglers to deliver individually.
const GROUP_TIMEOUT_MINUTES: i64 = 30;

/// Failed proactive attempts before the transcript-backed delivery becomes
/// passive and waits for the next successful user turn.
const MAX_DELIVERY_ATTEMPTS: u32 = 5;

const RETRY_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
const RETRY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);
const BACKGROUND_NOTIFICATION_SOURCE_EVENT_PREFIX: &str = "background-notification:";

fn background_batch_source_event_id(handle_ids: &[String]) -> String {
    let mut handles: Vec<&str> = handle_ids.iter().map(String::as_str).collect();
    handles.sort_unstable();
    let mut hasher = Sha256::new();
    for handle in handles {
        hasher.update(handle.len().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(handle.as_bytes());
    }
    format!(
        "{BACKGROUND_NOTIFICATION_SOURCE_EVENT_PREFIX}{}",
        hex::encode(hasher.finalize())
    )
}

fn background_prompt_source_event_id(handle_ids: &[String]) -> String {
    format!("{}:prompt", background_batch_source_event_id(handle_ids))
}

fn background_reanchor_source_event_id(delivery: &BackgroundNotificationDelivery) -> String {
    background_reanchor_source_event_id_for(&delivery.handle_ids, delivery.prompt_ordinal)
}

fn background_reanchor_source_event_id_for(handle_ids: &[String], prompt_ordinal: i64) -> String {
    if handle_ids.is_empty() {
        format!(
            "{BACKGROUND_NOTIFICATION_SOURCE_EVENT_PREFIX}legacy-prompt:{prompt_ordinal}:reanchor"
        )
    } else {
        format!(
            "{}:reanchor-after:{prompt_ordinal}",
            background_batch_source_event_id(handle_ids)
        )
    }
}

/// Results of a settling batch whose required headings the reply omitted.
struct CoverageGap {
    batch: usize,
    indices: Vec<usize>,
}

/// Per-actor retry cadence. Inbound work resets it; consecutive timer attempts
/// double it while notification work remains.
pub(super) struct BackgroundNotificationRetrySchedule {
    delay: std::time::Duration,
}

impl BackgroundNotificationRetrySchedule {
    pub(super) fn new() -> Self {
        Self {
            delay: RETRY_INITIAL_BACKOFF,
        }
    }

    pub(super) fn delay(&self) -> std::time::Duration {
        self.delay
    }

    pub(super) fn reset(&mut self) {
        self.delay = RETRY_INITIAL_BACKOFF;
    }

    pub(super) fn after_timer_attempt(&mut self, work_remains: bool) {
        self.delay = if work_remains {
            (self.delay * 2).min(RETRY_MAX_BACKOFF)
        } else {
            RETRY_INITIAL_BACKOFF
        };
    }
}

impl AgentActor {
    /// Accept one terminal event into the collecting stage. Dedup spans every
    /// stage, including the active ledger, so a re-published completion cannot
    /// leak into the next batch while its original delivery is retrying.
    pub(super) async fn handle_background_finished(&mut self, pending: PendingBackgroundResult) {
        if self
            .durable
            .session
            .state
            .background_notifications
            .knows_handle(&pending.handle_id)
        {
            debug!(
                session_id = %self.durable.session.id,
                handle_id = %pending.handle_id,
                "duplicate BackgroundJobFinished for handle; ignoring"
            );
            return;
        }

        // No label: for a detached `Bash` job it is the command line verbatim
        // (`PendingBackgroundResult::command` stores `label: command.clone()`),
        // so it routinely carries credentials — and logs are served over an
        // HTTP endpoint. `handle_id` already identifies the result, and the
        // label is reachable from the durable row for anyone who needs it.
        debug!(
            session_id = %self.durable.session.id,
            handle_id = %pending.handle_id,
            "accepted background result into notification pipeline"
        );

        let group = pending.group.as_ref().and_then(|key| {
            self.durable
                .session
                .state
                .background_notifications
                .groups
                .contains_key(key)
                .then(|| key.clone())
        });
        match group {
            Some(key) => {
                if let Some(group) = self
                    .durable
                    .session
                    .state
                    .background_notifications
                    .groups
                    .get_mut(&key)
                {
                    group.results.push(pending);
                }
            }
            None => self.buffer_background_result(pending),
        }

        self.release_ready_background_groups();
        self.persist_session_state_with_activity("background_result_finished")
            .await;
    }

    fn buffer_background_result(&mut self, pending: PendingBackgroundResult) {
        let buffer = &mut self
            .durable
            .session
            .state
            .background_notifications
            .buffered_results;
        if buffer.len() >= MAX_BUFFERED_RESULTS {
            let dropped = buffer.remove(0);
            warn!(
                session_id = %self.durable.session.id,
                dropped_handle_id = %dropped.handle_id,
                cap = MAX_BUFFERED_RESULTS,
                "background notification buffer full; dropping oldest result"
            );
        }
        buffer.push(pending);
    }

    /// Seal cohorts at a turn boundary, when no more grouped spawns can join
    /// that turn. A complete cohort becomes immediately eligible for release.
    pub(super) async fn seal_background_notification_groups(&mut self) {
        let now = chrono::Utc::now();
        let mut changed = false;
        for group in self
            .durable
            .session
            .state
            .background_notifications
            .groups
            .values_mut()
        {
            if !group.sealed {
                group.sealed = true;
                group.sealed_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.release_ready_background_groups();
            self.persist_session_state_with_activity("background_groups_sealed")
                .await;
        }
    }

    /// Move complete or timed-out cohorts into the uncommitted result buffer.
    /// Timed-out cohorts dissolve: members that finish later no longer find a
    /// group and therefore enter the buffer individually.
    fn release_ready_background_groups(&mut self) -> bool {
        let now = chrono::Utc::now();
        let timeout = chrono::Duration::minutes(GROUP_TIMEOUT_MINUTES);
        let ready: Vec<String> = self
            .durable
            .session
            .state
            .background_notifications
            .groups
            .iter()
            .filter(|(_, group)| group.is_ready(now, timeout))
            .map(|(name, _)| name.clone())
            .collect();
        let released = !ready.is_empty();

        for name in ready {
            let Some(group) = self
                .durable
                .session
                .state
                .background_notifications
                .groups
                .remove(&name)
            else {
                continue;
            };
            if group.is_partial() {
                debug!(
                    session_id = %self.durable.session.id,
                    group = %name,
                    have = group.results.len(),
                    expected = group.expected,
                    "background group timed out; releasing partial batch"
                );
            }
            for result in group.results {
                self.buffer_background_result(result);
            }
        }

        released
    }

    pub(super) fn has_background_notification_work(&self) -> bool {
        self.durable
            .session
            .state
            .background_notifications
            .has_work()
    }

    /// Timer edge for idle delivery/retry. It first releases any group whose
    /// barrier completed or expired, then advances either the active delivery
    /// or the next buffered batch.
    pub(super) async fn handle_background_notification_timer(&mut self) {
        if self.release_ready_background_groups() {
            self.persist_session_state_with_activity("background_groups_released")
                .await;
        }
        self.run_background_notification_delivery().await;
    }

    /// Start a buffered batch after ordinary actor work, but never jump ahead
    /// of queued user/completion messages and never retry an active delivery
    /// immediately behind an inbound turn. Active retries belong to the timer.
    pub(super) async fn maybe_start_background_notification(
        &mut self,
        mailbox: &mailbox::MailboxReceiver<AgentMessage>,
    ) {
        let notifications = &self.durable.session.state.background_notifications;
        if notifications.buffered_results.is_empty() || notifications.active_delivery.is_some() {
            return;
        }
        if matches!(
            mailbox.peek_priority(),
            Some(priority) if priority >= mailbox::MessagePriority::BackgroundJobFinished
        ) {
            return;
        }
        self.run_background_notification_delivery().await;
    }

    /// Advance the delivery stage. An active ledger always completes first;
    /// any fresh results remain buffered as the following batch.
    async fn run_background_notification_delivery(&mut self) {
        if self
            .durable
            .session
            .state
            .background_notifications
            .active_delivery
            .is_some()
        {
            self.retry_background_notification_delivery().await;
            return;
        }

        let pending = std::mem::take(
            &mut self
                .durable
                .session
                .state
                .background_notifications
                .buffered_results,
        );
        if pending.is_empty() {
            return;
        }

        let (handle_ids, result_labels): (Vec<String>, Vec<String>) = pending
            .iter()
            .map(|result| (result.handle_id.clone(), result.label.clone()))
            .unzip();
        let content = baybo_context::prompts::background_notification::build_notification_content(
            &pending,
            &self.volatile.workspace,
        );
        let source_event_id = background_prompt_source_event_id(&handle_ids);

        // The prompt row and its batch idempotency key land atomically. Raw
        // results own analysed-report delivery until this prompt is durable.
        let append_outcome = match self
            .volatile
            .agent_loop
            .append_background_notification_prompt_once(content.clone(), &source_event_id)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                warn!(
                    session_id = %self.durable.session.id,
                    error = %error,
                    "background notification prompt failed to persist; restoring buffered batch"
                );
                self.durable
                    .session
                    .state
                    .background_notifications
                    .buffered_results = pending;
                return;
            }
        };
        let mut prompt_ordinal = append_outcome.ordinal();
        if !append_outcome.was_inserted() {
            let prompt_active = match self
                .volatile
                .session_manager
                .active_index_of_ordinal(&self.durable.session.id, prompt_ordinal)
                .await
            {
                Ok(index) => index.is_some(),
                Err(error) => {
                    warn!(
                        session_id = %self.durable.session.id,
                        error = %error,
                        "could not verify recovered background notification prompt"
                    );
                    self.durable
                        .session
                        .state
                        .background_notifications
                        .buffered_results = pending;
                    return;
                }
            };
            if !prompt_active {
                let reanchor_source_event_id =
                    background_reanchor_source_event_id_for(&handle_ids, prompt_ordinal);
                let reanchored = match self
                    .volatile
                    .agent_loop
                    .append_background_notification_prompt_once(
                        content.clone(),
                        &reanchor_source_event_id,
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        warn!(
                            session_id = %self.durable.session.id,
                            error = %error,
                            "background notification prompt re-anchor failed"
                        );
                        self.durable
                            .session
                            .state
                            .background_notifications
                            .buffered_results = pending;
                        return;
                    }
                };
                prompt_ordinal = reanchored.ordinal();
            }
        }

        // The acknowledgement rides the control-event plane: shown in the chat
        // transcript, never in `session_messages`, so it cannot reach the model.
        // As an assistant row it did — and a fixed sentence sitting in the
        // transcript as the model's own last words is a template it imitates,
        // which is how users saw the same line twice in a row, once from here
        // and once tacked onto the end of the model's next answer.
        //
        // Gated on the prompt insert rather than carrying an idempotency key of
        // its own: one acknowledgement per analysis turn is exactly the rule,
        // and the prompt row is already the durable record of "this batch is
        // new". A crash replay recovers the prompt as `Existing` and stays
        // quiet. This is also the fix for a duplicate the old key could not
        // prevent — it hashed the batch's handle set, so a result joining the
        // batch between a failed attempt and its retry produced a second
        // acknowledgement for the same work.
        if append_outcome.was_inserted() {
            self.announce_background_completion(&pending, prompt_ordinal)
                .await;
        }

        self.durable
            .session
            .state
            .background_notifications
            .active_delivery = Some(BackgroundNotificationDelivery {
            handle_ids,
            result_labels,
            content: content.clone(),
            prompt_ordinal,
            failed_attempts: 0,
        });

        // Commit "buffer emptied + ledger opened" before inference. If this
        // save fails, the in-memory ledger stays active and the next timer
        // persists it before retrying. A crash in that narrow window restores
        // the raw batch, whose stable source-event key recovers this ordinal.
        if !self
            .persist_session_state_with_activity("background_notification_started")
            .await
        {
            return;
        }
        self.drive_background_notification_turn(content).await;
    }

    /// Persist and dispatch the "work finished, reviewing it now" notice for a
    /// freshly opened batch. Anchored after the batch's prompt row so the chat
    /// view interleaves it ahead of the analysed report on reload as well as
    /// live. Best-effort, like every control event: a failed write costs the
    /// notice its place on reload, never the report.
    async fn announce_background_completion(
        &self,
        pending: &[PendingBackgroundResult],
        prompt_ordinal: i64,
    ) {
        let text = baybo_context::prompts::background_notification::build_completion_reply(pending);
        let durable_id = self
            .persist_control_event(
                prompt_ordinal,
                ControlEventKind::NoticeInfo,
                &text,
                chrono::Utc::now(),
                "",
            )
            .await;
        self.send_response(
            AgentOutput {
                session_id: self.durable.session.id.clone(),
                user_id: self.durable.session.user.id.clone(),
                channel: self.durable.session.channel.clone(),
                event: AgentEvent::Notice {
                    level: NoticeLevel::Info,
                    text,
                    mid_turn: false,
                    durable_id,
                },
            },
            "background_completion",
        )
        .await;
    }

    /// Re-drive a transcript-backed delivery without rollback. A cue row
    /// restores a user-role request tail after partial failed attempts. If
    /// compaction superseded the original prompt, the frozen ledger content is
    /// re-appended and the ordinal is updated first.
    async fn retry_background_notification_delivery(&mut self) {
        if !self
            .persist_session_state_with_activity("background_notification_retrying")
            .await
        {
            return;
        }
        let Some(delivery) = self
            .durable
            .session
            .state
            .background_notifications
            .active_delivery
            .clone()
        else {
            return;
        };

        let prompt_active = match self
            .volatile
            .session_manager
            .active_index_of_ordinal(&self.durable.session.id, delivery.prompt_ordinal)
            .await
        {
            Ok(index) => index.is_some(),
            Err(error) => {
                warn!(
                    session_id = %self.durable.session.id,
                    error = %error,
                    "could not verify the background notification prompt row"
                );
                self.record_background_notification_failure("prompt row verification failed")
                    .await;
                return;
            }
        };

        if !prompt_active {
            // Compaction re-inserted the kept slice at fresh ordinals, so the
            // recorded prompt ordinal dangles. Re-append the frozen content as
            // a fresh persisted row so the request carries the prompt. (When it
            // IS active, nothing to do here — `drive` arms the request-time cue
            // that restores a user-role tail if the prior attempt left one.)
            let reanchor_content = delivery.content.clone();
            let source_event_id = background_reanchor_source_event_id(&delivery);
            let new_ordinal = self
                .volatile
                .agent_loop
                .append_background_notification_prompt_once(reanchor_content, &source_event_id)
                .await
                .map(|outcome| outcome.ordinal());
            let new_ordinal = match new_ordinal {
                Ok(ordinal) => ordinal,
                Err(error) => {
                    warn!(
                        session_id = %self.durable.session.id,
                        error = %error,
                        "active background notification prompt re-anchor failed"
                    );
                    self.record_background_notification_failure(
                        "re-anchored prompt row failed to persist",
                    )
                    .await;
                    return;
                }
            };
            if let Some(active) = self
                .durable
                .session
                .state
                .background_notifications
                .active_delivery
                .as_mut()
            {
                active.prompt_ordinal = new_ordinal;
            }
            self.persist_session_state_with_activity("background_notification_reanchored")
                .await;
        }

        self.drive_background_notification_turn(delivery.content)
            .await;
    }

    /// Count failures in both transcript preparation and inference against the
    /// same cap. Otherwise a partially broken store could keep an idle actor
    /// resident forever while never reaching the model.
    async fn record_background_notification_failure(&mut self, reason: &'static str) {
        let failed_attempts = match self
            .durable
            .session
            .state
            .background_notifications
            .active_delivery
            .as_mut()
        {
            Some(delivery) => {
                delivery.failed_attempts = delivery.failed_attempts.saturating_add(1);
                delivery.failed_attempts
            }
            None => return,
        };

        if failed_attempts >= MAX_DELIVERY_ATTEMPTS {
            warn!(
                session_id = %self.durable.session.id,
                failed_attempts,
                reason,
                "background notification delivery kept failing; degrading to passive delivery"
            );
            self.durable
                .session
                .state
                .background_notifications
                .active_delivery = None;
        }
        self.persist_session_state_with_activity("background_notification_failed")
            .await;
    }

    async fn drive_background_notification_turn(&mut self, content: Vec<ContentBlock>) {
        // The persisted/public turn kind retains its historical name for data
        // compatibility; this path now covers every background-turn kind. Its
        // delta sink exposes reasoning, tool progress, and answer prose before
        // the canonical final Message.
        let response_tx = self.volatile.response_tx.clone();
        // Arm the request-time retry cue for the duration of the turn. It rides
        // a request only while the transcript tail is an assistant row (a prior
        // attempt's cancelled salvage) — so the fresh path, where the tail is
        // the just-appended user-role prompt, is unaffected, and a mid-turn
        // iteration whose tail becomes a tool result drops it automatically.
        // This single arm covers every entry (fresh drain, timer retry, crash
        // replay), so no caller has to reason about the tail.
        self.volatile.agent_loop.set_notification_cue(true);
        let result = self
            .run_agent_loop(
                TurnInput::SubagentNotification { content },
                None,
                Some(response_tx),
                None,
                None,
                None,
                None,
            )
            .await;
        self.volatile.agent_loop.set_notification_cue(false);

        match result {
            Ok(response) => {
                let failed_attempts = self
                    .durable
                    .session
                    .state
                    .background_notifications
                    .active_delivery
                    .as_ref()
                    .map(|delivery| delivery.failed_attempts)
                    .unwrap_or(0);
                if let Some(gap) = self.unreported_background_results(&response.content) {
                    warn!(
                        session_id = %self.durable.session.id,
                        batch = gap.batch,
                        unreported_indices = ?gap.indices,
                        "background notification reply omitted required result sections"
                    );
                }
                self.durable
                    .session
                    .state
                    .background_notifications
                    .active_delivery = None;
                self.persist_session_state_with_activity("background_notification_delivered")
                    .await;

                if is_blank_reply(&response.content) {
                    if failed_attempts > 0 {
                        warn!(
                            session_id = %self.durable.session.id,
                            failed_attempts,
                            "background notification retry produced no output; suppressing send"
                        );
                    } else {
                        debug!(
                            session_id = %self.durable.session.id,
                            "background notification produced no output; suppressing send"
                        );
                    }
                    return;
                }
                self.send_response(response.into(), "background_notification")
                    .await;
            }
            Err(error) => {
                if is_turn_cancelled(&error) {
                    // A cancelled turn (shutdown / `/stop`) is not a delivery
                    // failure: log it and leave the ledger untouched so a
                    // cancellation can't degrade delivery toward passive.
                    info!(
                        session_id = %self.durable.session.id,
                        "turn cancelled"
                    );
                } else {
                    error!(
                        session_id = %self.durable.session.id,
                        error = %error,
                        "background notification turn failed"
                    );
                    self.record_background_notification_failure("notification turn failed")
                        .await;
                }
            }
        }
    }

    /// Which results of the batch about to settle are missing their required
    /// heading in `reply`, or `None` when the batch is fully covered, carries
    /// no auditable labels (a ledger written before the audit), or is a single
    /// result (never asked for a heading).
    ///
    /// Reports indices, never labels. A label is the task summary for a
    /// subagent but the **command line** for a detached `Bash` job
    /// (`PendingBackgroundResult::command` stores `label: command.clone()`),
    /// which routinely carries credentials — and the only consumer here is a
    /// log line, served over an HTTP endpoint.
    ///
    /// Observation only: every caller still settles. Re-delivering the missing
    /// results would need a narrowed prompt built from fresh
    /// `PendingBackgroundResult`s under its own source-event key, not a
    /// narrowed `handle_ids` beside the frozen whole-batch `content` (that key
    /// is also the crash-replay idempotency key and the terminal-event dedup
    /// set). Until then this is what makes "reported 1 of 3" visible at all:
    /// the settle path is otherwise silent, and only `is_blank_reply` — which
    /// a partial report passes — stands between a batch and its retirement.
    fn unreported_background_results(&self, reply: &[ContentBlock]) -> Option<CoverageGap> {
        let delivery = self
            .durable
            .session
            .state
            .background_notifications
            .active_delivery
            .as_ref()?;
        let reply_text = reply
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let indices = baybo_context::prompts::background_notification::unreported_result_indices(
            &delivery.result_labels,
            &reply_text,
        );
        if indices.is_empty() {
            return None;
        }
        Some(CoverageGap {
            batch: delivery.result_labels.len(),
            indices,
        })
    }

    /// A successful user reply already saw the active prompt row in its
    /// request. Settle that delivery passively so the timer cannot produce a
    /// duplicate proactive report. Stopped or blank turns did not deliver a
    /// usable reply and leave the ledger open.
    pub(super) async fn settle_background_notification_after_user_turn(
        &mut self,
        stopped: bool,
        response_content: &[ContentBlock],
    ) {
        if stopped
            || is_blank_reply(response_content)
            || self
                .durable
                .session
                .state
                .background_notifications
                .active_delivery
                .is_none()
        {
            return;
        }

        debug!(
            session_id = %self.durable.session.id,
            "user turn completed over active background notification; settling passively"
        );
        // A ledger opens and clears inside one actor message, so reaching here
        // means the proactive attempt failed, was cancelled, or was cut short
        // by a restart — never that a healthy delivery is still pending.
        //
        // "Cancelled" is why this says nothing about how much reached the
        // user: `/stop` is deliberately not a delivery failure (it leaves
        // `failed_attempts` alone above), and that turn streamed through
        // `response_tx`, so the user may well have read the first sections
        // before stopping it. The warning claims only what it can see — this
        // batch is retiring without a complete report.
        if let Some(gap) = self.unreported_background_results(response_content) {
            warn!(
                session_id = %self.durable.session.id,
                batch = gap.batch,
                unreported_indices = ?gap.indices,
                "background notification reply omitted required result sections"
            );
        }
        self.durable
            .session
            .state
            .background_notifications
            .active_delivery = None;
        self.persist_session_state_with_activity("background_notification_settled")
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_source_event_ids_are_stable_and_operation_scoped() {
        let first = vec!["bg-b".to_string(), "bg-a".to_string()];
        let reordered = vec!["bg-a".to_string(), "bg-b".to_string()];
        let different = vec!["bg-a".to_string()];

        assert_eq!(
            background_prompt_source_event_id(&first),
            background_prompt_source_event_id(&reordered)
        );
        assert_ne!(
            background_prompt_source_event_id(&first),
            background_prompt_source_event_id(&different)
        );

        let delivery = BackgroundNotificationDelivery {
            handle_ids: first,
            result_labels: Vec::new(),
            content: Vec::new(),
            prompt_ordinal: 7,
            failed_attempts: 1,
        };
        let reanchor = background_reanchor_source_event_id(&delivery);
        assert_ne!(
            reanchor,
            background_prompt_source_event_id(&delivery.handle_ids)
        );

        let legacy = BackgroundNotificationDelivery {
            handle_ids: Vec::new(),
            result_labels: Vec::new(),
            content: Vec::new(),
            prompt_ordinal: 11,
            failed_attempts: 1,
        };
        assert_eq!(
            background_reanchor_source_event_id(&legacy),
            "background-notification:legacy-prompt:11:reanchor"
        );
    }
}
