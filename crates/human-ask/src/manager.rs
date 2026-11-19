//! [`HumanAskManager`]: the verbs every other crate calls to move an ask out
//! of a state. Each verb persists through a claim first and only then
//! touches memory; the claim is the arbiter between concurrent verbs.

use std::sync::Arc;

use baybo_model::SessionId;
use baybo_model::human_ask::{AskId, AskKind, AskResolution, ResumeRequest, resumable};
use baybo_store::HumanAskRow;

use crate::core::AskCore;
use crate::error::{HumanAskError, Result};
use crate::ports::{ActorStopCost, LivePendingAsks, ResumeSink};
use crate::queue::{AskQueues, PendingAsk};

pub struct HumanAskManagerConfig {
    pub core: Arc<AskCore>,
    /// The same queues the channels fetch from.
    pub queues: AskQueues,
    pub resume_sink: Arc<dyn ResumeSink>,
}

pub struct HumanAskManager {
    core: Arc<AskCore>,
    queues: AskQueues,
    resume_sink: Arc<dyn ResumeSink>,
}

/// One ask [`HumanAskManager::settle_unapplied`] needs its caller to put on
/// the transcript: append the result `resolution` means for `tool_call_id`
/// (or re-execute an approved call), then
/// [`HumanAskManager::mark_applied`]. The row stays unapplied until then,
/// so a settlement that fails part-way is retried whole by the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledAsk {
    pub ask_id: AskId,
    pub tool_call_id: String,
    pub kind: AskKind,
    /// `Abandoned` for an ask nobody answered, the settlement's own claim
    /// included.
    pub resolution: AskResolution,
}

/// Counts from one [`HumanAskManager::reload_pending`] pass, for the boot log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReloadReport {
    /// Pending resumable asks put back on a queue as dead entries.
    pub requeued: usize,
    /// Resolved resumable asks handed to the resume sink.
    pub resumed: usize,
    /// Pending non-resumable asks abandoned.
    pub abandoned: usize,
    /// Resolved non-resumable asks stamped applied.
    pub applied: usize,
    /// Rows whose reload failed and were left as they were.
    pub failed: usize,
}

impl HumanAskManager {
    pub fn from_config(config: HumanAskManagerConfig) -> Self {
        let HumanAskManagerConfig {
            core,
            queues,
            resume_sink,
        } = config;
        Self {
            core,
            queues,
            resume_sink,
        }
    }

    pub async fn get(&self, ask_id: &AskId) -> Result<Option<HumanAskRow>> {
        self.core.get(ask_id).await
    }

    /// A human (or policy) decided `ask_id`. Claims the resolution, then
    /// removes the queue entry and hands the decision to the live waiter, or
    /// — for a dead resumable ask outside an issue session — requests a
    /// Resume turn. `Err(AskClosed)` when another verb claimed it first, or
    /// when the ask is a dead non-resumable one, which this call closes as
    /// `Abandoned`.
    pub async fn resolve(&self, ask_id: &AskId, resolution: AskResolution) -> Result<()> {
        let mut row = self
            .get(ask_id)
            .await?
            .ok_or_else(|| HumanAskError::NotFound {
                ask_id: ask_id.clone(),
            })?;
        if !fits(row.kind, &resolution) {
            return Err(HumanAskError::KindMismatch {
                ask_id: ask_id.clone(),
                kind: row.kind,
            });
        }
        if self.close_if_unanswerable(&mut row).await?
            || !self.core.claim(&mut row, resolution.clone()).await?
        {
            return Err(HumanAskError::AskClosed {
                ask_id: ask_id.clone(),
            });
        }

        let delivered = match self.queues.get(&row.channel) {
            Some(queue) => queue
                .resolve(ask_id, resolution)
                .is_some_and(|removed| removed.delivered),
            None => {
                tracing::warn!(
                    %ask_id,
                    channel = %row.channel,
                    "resolved an ask whose channel has no queue"
                );
                false
            }
        };
        if !delivered && resumable(row.kind, row.phase) {
            self.request_resume(&row);
        }
        Ok(())
    }

    /// Abandon one ask. `Ok(false)` when it was not pending (absent or
    /// already resolved). A live waiter sees `Abandoned`.
    pub async fn abandon(&self, ask_id: &AskId) -> Result<bool> {
        let Some(mut row) = self.get(ask_id).await? else {
            return Ok(false);
        };
        self.abandon_row(&mut row).await
    }

    /// Abandon every pending ask of a session, live or dead. Returns how many
    /// this call claimed.
    pub async fn abandon_for_session(&self, session_id: &SessionId) -> Result<usize> {
        let mut claimed = 0;
        for mut row in self.unapplied_for_session(session_id).await? {
            if row.is_pending() && self.abandon_row(&mut row).await? {
                claimed += 1;
            }
        }
        Ok(claimed)
    }

    /// The turn-cancel claim for one tool call, made by whoever is about to
    /// append that call's result, immediately before the append. Its tool
    /// future is gone, so no answer to it can be applied any more: pending
    /// asks are claimed `Abandoned`, and every unapplied ask of the call —
    /// one answered in the same instant included — is stamped applied,
    /// because the caller's append is what consumes it. Returns whether the
    /// call had any unapplied ask.
    pub async fn abandon_for_tool_call(
        &self,
        session_id: &SessionId,
        tool_call_id: &str,
    ) -> Result<bool> {
        let rows: Vec<HumanAskRow> = self
            .unapplied_for_session(session_id)
            .await?
            .into_iter()
            .filter(|row| row.tool_call_id == tool_call_id)
            .collect();
        let found = !rows.is_empty();
        for mut row in rows {
            if row.is_pending() {
                self.abandon_row(&mut row).await?;
            }
            self.core.mark_applied(&row.ask_id).await?;
        }
        Ok(found)
    }

    /// Settle a session's unapplied asks before a turn builds on its
    /// transcript. Every resumable one comes back as a [`SettledAsk`] for
    /// the caller to append and stamp: a pending dead ask is claimed
    /// `Abandoned` first, a resolved one carries its resolution. A
    /// non-resumable one's call already has a result (the tool's own, the
    /// loop's cancel fill, or hydration's crash fill), so it is closed and
    /// stamped applied here and reported as nothing. A live ask cannot exist
    /// here — it is logged and left alone.
    pub async fn settle_unapplied(&self, session_id: &SessionId) -> Result<Vec<SettledAsk>> {
        let mut settled = Vec::new();
        for mut row in self.unapplied_for_session(session_id).await? {
            if row.is_pending() {
                if self.is_live(&row) {
                    tracing::warn!(
                        ask_id = %row.ask_id,
                        "settling a session with a live ask; leaving it parked"
                    );
                    continue;
                }
                if !self.abandon_row(&mut row).await? {
                    // Lost the claim to a concurrent verb: read what it wrote.
                    match self.get(&row.ask_id).await? {
                        Some(current) if current.applied_at.is_none() => row = current,
                        _ => continue,
                    }
                }
            }
            if let Some(next) = self.settle_resolved(row).await? {
                settled.push(next);
            }
        }
        Ok(settled)
    }

    pub async fn mark_applied(&self, ask_id: &AskId) -> Result<bool> {
        self.core.mark_applied(ask_id).await
    }

    /// Boot: rebuild the queues from the ledger. Pending resumable asks are
    /// re-enqueued as dead entries (publishing `Raised`, but no observer
    /// `Raised` — the ask was already announced); resolved resumable ones are
    /// handed to the resume sink without being shown again; pending
    /// non-resumable ones are abandoned (observers see that resolution) and
    /// resolved ones stamped applied. Idempotent. The gateway's boot step:
    /// a one-shot process does not reload, since it would run other sessions'
    /// Resume turns and cut them off at its own teardown.
    pub async fn reload_pending(&self) -> Result<ReloadReport> {
        let mut report = ReloadReport::default();
        for row in self.core.list_unapplied().await? {
            let ask_id = row.ask_id.clone();
            if let Err(e) = self.reload_row(row, &mut report).await {
                tracing::warn!(%ask_id, error = %e, "failed to reload a human ask");
                report.failed += 1;
            }
        }
        Ok(report)
    }

    /// One row of [`Self::reload_pending`]. A row that fails is left as it
    /// was for the next boot or its session's next settlement; it must not
    /// keep the rows after it off their queues.
    async fn reload_row(&self, mut row: HumanAskRow, report: &mut ReloadReport) -> Result<()> {
        match (row.is_pending(), resumable(row.kind, row.phase)) {
            (true, true) => {
                let queue = self.queues.get_or_create(&row.channel);
                if !queue.contains(&row.ask_id) {
                    queue.push_dead(PendingAsk::from_row(&row));
                    report.requeued += 1;
                }
            }
            (true, false) => {
                if self.abandon_row(&mut row).await? {
                    report.abandoned += 1;
                }
                self.core.mark_applied(&row.ask_id).await?;
            }
            (false, true) => {
                self.request_resume(&row);
                report.resumed += 1;
            }
            (false, false) => {
                if self.core.mark_applied(&row.ask_id).await? {
                    report.applied += 1;
                }
            }
        }
        Ok(())
    }

    /// LLM `tool_use` ids of the session's unapplied resumable asks — the
    /// calls hydration must leave dangling. Read from the ledger, not the
    /// queue: a just-answered dead ask is already off the queue.
    pub async fn unapplied_tool_use_ids(&self, session_id: &SessionId) -> Result<Vec<String>> {
        Ok(self
            .unapplied_for_session(session_id)
            .await?
            .into_iter()
            .filter(|row| resumable(row.kind, row.phase))
            .map(|row| row.tool_call_id)
            .collect())
    }

    /// The session's asks not yet applied to its transcript, live or dead.
    pub async fn unapplied_for_session(&self, session_id: &SessionId) -> Result<Vec<HumanAskRow>> {
        self.core.list_unapplied_for_session(session_id).await
    }

    /// The row a Resume turn should start from for the answered `ask_id`, or
    /// `None` when there is nothing to do yet: the ledger does not have it,
    /// it is not resolved, a door's prelude already applied it, or its turn
    /// still waits on another answer (see [`Self::awaits_more_answers`]).
    pub async fn resume_ready(&self, ask_id: &AskId) -> Result<Option<HumanAskRow>> {
        let Some(row) = self.get(ask_id).await? else {
            tracing::warn!(%ask_id, "resume for an ask the ledger does not have");
            return Ok(None);
        };
        if row.applied_at.is_some() || row.is_pending() {
            return Ok(None);
        }
        if self.awaits_more_answers(&row).await? {
            tracing::debug!(%ask_id, turn_id = %row.turn_id, "resume waits for the turn's other answers");
            return Ok(None);
        }
        Ok(Some(row))
    }

    /// Whether the turn that raised `row` still has a resumable ask nobody
    /// has answered. Answering one of several asks a turn parked on must not
    /// carry the turn on: its settlement would abandon the others. The
    /// Resume door waits for the last answer, as the live batch would.
    ///
    /// Scoped to the raising turn while the settlement it guards applies the
    /// whole session's asks. The two agree: a session runs one turn at a
    /// time and each turn's prelude settles what the previous one left, so
    /// only the newest turn can still have a pending ask.
    async fn awaits_more_answers(&self, row: &HumanAskRow) -> Result<bool> {
        Ok(self
            .unapplied_for_session(&row.session_id)
            .await?
            .iter()
            .any(|sibling| {
                sibling.turn_id == row.turn_id
                    && sibling.ask_id != row.ask_id
                    && sibling.is_pending()
                    && resumable(sibling.kind, sibling.phase)
            }))
    }

    /// Whether a tool future is parked on an ask of `session_id`: what a
    /// test waits for before acting on a parked turn.
    #[cfg(any(test, feature = "test-support"))]
    pub fn has_live_pending_ask(&self, session_id: &SessionId) -> bool {
        LivePendingAsks::actor_stop_cost(self, session_id) != ActorStopCost::Free
    }

    fn is_live(&self, row: &HumanAskRow) -> bool {
        self.queues
            .get(&row.channel)
            .and_then(|queue| queue.liveness(&row.ask_id))
            .unwrap_or(false)
    }

    /// A pending ask nothing can apply an answer to: not resumable, and the
    /// tool future that raised it is gone. Close it as abandoned so a late
    /// answer is refused instead of recorded as a decision nothing carries
    /// out. `Ok(true)` when `row` was such an ask.
    async fn close_if_unanswerable(&self, row: &mut HumanAskRow) -> Result<bool> {
        if !row.is_pending() || resumable(row.kind, row.phase) || self.is_live(row) {
            return Ok(false);
        }
        self.abandon_row(row).await?;
        self.core.mark_applied(&row.ask_id).await?;
        Ok(true)
    }

    async fn abandon_row(&self, row: &mut HumanAskRow) -> Result<bool> {
        if !self.core.claim(row, AskResolution::Abandoned).await? {
            return Ok(false);
        }
        if let Some(queue) = self.queues.get(&row.channel) {
            let _ = queue.abandon(&row.ask_id);
        }
        Ok(true)
    }

    async fn settle_resolved(&self, row: HumanAskRow) -> Result<Option<SettledAsk>> {
        if !resumable(row.kind, row.phase) {
            self.core.mark_applied(&row.ask_id).await?;
            return Ok(None);
        }
        let resolution = row.resolution.unwrap_or_else(|| {
            tracing::warn!(ask_id = %row.ask_id, "resolved ask without a resolution; abandoning");
            AskResolution::Abandoned
        });
        Ok(Some(SettledAsk {
            ask_id: row.ask_id,
            tool_call_id: row.tool_call_id,
            kind: row.kind,
            resolution,
        }))
    }

    fn request_resume(&self, row: &HumanAskRow) {
        self.resume_sink.request_resume(ResumeRequest {
            session_id: row.session_id.clone(),
            ask_id: row.ask_id.clone(),
        });
    }
}

impl LivePendingAsks for HumanAskManager {
    /// The session's live asks across every channel queue.
    fn actor_stop_cost(&self, session_id: &SessionId) -> ActorStopCost {
        self.queues
            .all()
            .iter()
            .map(|queue| queue.actor_stop_cost(session_id))
            .fold(ActorStopCost::Free, ActorStopCost::plus)
    }
}

/// Whether `resolution` is something an ask of `kind` can resolve to.
fn fits(kind: AskKind, resolution: &AskResolution) -> bool {
    match resolution {
        AskResolution::Approval(_) => kind == AskKind::Approval,
        AskResolution::Answered(_) | AskResolution::Declined => kind == AskKind::Question,
        AskResolution::Abandoned | AskResolution::PolicyDenied => true,
        // Only the gate's own deadline produces it, never a verb.
        AskResolution::TimedOut => false,
    }
}
