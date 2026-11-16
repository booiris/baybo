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

/// What [`HumanAskManager::settle_unapplied`] needs its caller to put on the
/// transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// A pending ask the settlement abandoned (already stamped applied):
    /// append the synthetic "nobody answered" result.
    Abandoned { tool_call_id: String, kind: AskKind },
    /// A resolved resumable ask: append (or re-execute for) `resolution`,
    /// then [`HumanAskManager::mark_applied`].
    Apply {
        ask_id: AskId,
        tool_call_id: String,
        kind: AskKind,
        resolution: AskResolution,
    },
}

/// Whether [`HumanAskManager::reload_pending`] re-drives answered asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadScope {
    /// The gateway: answered resumable asks go to the resume sink.
    ResumeAnswered,
    /// A one-shot process that tears down after its own turn: answered asks
    /// are left for the gateway's next boot or the session's next turn.
    LeaveAnswered,
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
    /// Resume turn. `Err(AskClosed)` when another verb claimed it first.
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
        if !self.core.claim(&mut row, resolution.clone()).await? {
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
            self.request_resume(&row).await;
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
    /// transcript. Pending dead asks are abandoned (and stamped applied) and
    /// come back as [`Settled::Abandoned`] when resumable; a non-resumable
    /// one's call already has a result (the loop's cancel fill, or
    /// hydration's crash fill), so it is reported as nothing. Resolved
    /// resumable ones come back as [`Settled::Apply`]; resolved
    /// non-resumable ones are stamped applied and reported as nothing. A
    /// live ask cannot exist here — it is logged and left alone.
    pub async fn settle_unapplied(&self, session_id: &SessionId) -> Result<Vec<Settled>> {
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
                if self.abandon_row(&mut row).await? {
                    self.core.mark_applied(&row.ask_id).await?;
                    if !resumable(row.kind, row.phase) {
                        continue;
                    }
                    settled.push(Settled::Abandoned {
                        tool_call_id: row.tool_call_id,
                        kind: row.kind,
                    });
                    continue;
                }
                // Lost the claim to a concurrent verb: read what it wrote.
                match self.get(&row.ask_id).await? {
                    Some(current) if current.applied_at.is_none() => row = current,
                    _ => continue,
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
    /// handed to the resume sink without being shown again, unless `scope`
    /// is [`ReloadScope::LeaveAnswered`]; pending non-resumable ones are
    /// abandoned (observers see that resolution) and resolved ones stamped
    /// applied. Idempotent.
    pub async fn reload_pending(&self, scope: ReloadScope) -> Result<ReloadReport> {
        let mut report = ReloadReport::default();
        for row in self.core.list_unapplied().await? {
            let resumable = resumable(row.kind, row.phase);
            match (row.is_pending(), resumable) {
                (true, true) => {
                    let queue = self.queues.get_or_create(&row.channel);
                    if !queue.contains(&row.ask_id) {
                        queue.push_dead(PendingAsk::from_row(&row));
                        report.requeued += 1;
                    }
                }
                (true, false) => {
                    let mut row = row;
                    if self.abandon_row(&mut row).await? {
                        report.abandoned += 1;
                    }
                    self.core.mark_applied(&row.ask_id).await?;
                }
                (false, true) => {
                    if scope == ReloadScope::ResumeAnswered && self.request_resume(&row).await {
                        report.resumed += 1;
                    }
                }
                (false, false) => {
                    if self.core.mark_applied(&row.ask_id).await? {
                        report.applied += 1;
                    }
                }
            }
        }
        Ok(report)
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

    /// Whether the turn that raised `row` still has a resumable ask nobody
    /// has answered. Answering one of several asks a turn parked on must not
    /// carry the turn on: its settlement would abandon the others. The
    /// Resume door waits for the last answer, as the live batch would.
    pub async fn awaits_more_answers(&self, row: &HumanAskRow) -> Result<bool> {
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
        self.actor_stop_cost(session_id) != ActorStopCost::Free
    }

    /// What stopping the session's actor would cost its live asks, across
    /// every channel queue.
    pub fn actor_stop_cost(&self, session_id: &SessionId) -> ActorStopCost {
        self.queues
            .all()
            .iter()
            .map(|queue| queue.actor_stop_cost(session_id))
            .max()
            .unwrap_or(ActorStopCost::Free)
    }

    fn is_live(&self, row: &HumanAskRow) -> bool {
        self.queues
            .get(&row.channel)
            .and_then(|queue| queue.liveness(&row.ask_id))
            .unwrap_or(false)
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

    async fn settle_resolved(&self, row: HumanAskRow) -> Result<Option<Settled>> {
        if !resumable(row.kind, row.phase) {
            self.core.mark_applied(&row.ask_id).await?;
            return Ok(None);
        }
        let Some(resolution) = row.resolution else {
            tracing::warn!(ask_id = %row.ask_id, "resolved ask without a resolution; abandoning");
            self.core.mark_applied(&row.ask_id).await?;
            return Ok(Some(Settled::Abandoned {
                tool_call_id: row.tool_call_id,
                kind: row.kind,
            }));
        };
        Ok(Some(Settled::Apply {
            ask_id: row.ask_id,
            tool_call_id: row.tool_call_id,
            kind: row.kind,
            resolution,
        }))
    }

    /// Hand a resolved dead ask to the resume sink unless its session
    /// resumes some other way. Returns whether a request was sent.
    async fn request_resume(&self, row: &HumanAskRow) -> bool {
        if !self.resume_sink.resumes_on_answer(&row.session_id).await {
            return false;
        }
        self.resume_sink.request_resume(ResumeRequest {
            session_id: row.session_id.clone(),
            ask_id: row.ask_id.clone(),
        });
        true
    }
}

impl LivePendingAsks for HumanAskManager {
    fn actor_stop_cost(&self, session_id: &SessionId) -> ActorStopCost {
        HumanAskManager::actor_stop_cost(self, session_id)
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
