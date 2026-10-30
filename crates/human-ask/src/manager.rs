//! [`HumanAskManager`]: the verbs every other crate calls to move an ask out
//! of a state. Each verb persists through a claim first and only then
//! touches memory; the claim is the arbiter between concurrent verbs.

use std::sync::Arc;

use baybo_model::SessionId;
use baybo_model::human_ask::{AskId, AskKind, AskResolution, ResumeRequest, resumable};
use baybo_store::HumanAskRow;
use chrono::Utc;

use crate::core::{AskCore, AskEvent};
use crate::error::{HumanAskError, Result};
use crate::ports::{LivePendingAsks, ResumeSink};
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

/// What [`HumanAskManager::abandon_for_tool_call`] found for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallAbandon {
    /// The call has no unapplied ask; append its result as usual.
    NoAsk,
    /// Its asks were abandoned and stamped applied; append the denial.
    Abandoned,
    /// One of its asks was answered first; append nothing and let the next
    /// settlement apply the answer.
    Answered,
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
        Ok(self.core.store().get(ask_id).await?)
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
        if !self.claim(&mut row, resolution.clone()).await? {
            return Err(HumanAskError::AskClosed {
                ask_id: ask_id.clone(),
            });
        }
        self.core
            .notify(&row, AskEvent::Resolved(resolution.clone()))
            .await;

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
        for mut row in self.unapplied(session_id).await? {
            if row.is_pending() && self.abandon_row(&mut row).await? {
                claimed += 1;
            }
        }
        Ok(claimed)
    }

    /// The turn-cancel claim for one tool call, made by whoever is about to
    /// append that call's result, immediately before the append. Abandoned
    /// asks are stamped applied here because the caller's append is what
    /// consumes them.
    pub async fn abandon_for_tool_call(
        &self,
        session_id: &SessionId,
        tool_call_id: &str,
    ) -> Result<ToolCallAbandon> {
        let rows: Vec<HumanAskRow> = self
            .unapplied(session_id)
            .await?
            .into_iter()
            .filter(|row| row.tool_call_id == tool_call_id)
            .collect();
        if rows.is_empty() {
            return Ok(ToolCallAbandon::NoAsk);
        }
        let mut answered = false;
        for mut row in rows {
            if row.is_pending() && self.abandon_row(&mut row).await? {
                self.core.mark_applied(&row.ask_id).await?;
            } else {
                answered = true;
            }
        }
        Ok(if answered {
            ToolCallAbandon::Answered
        } else {
            ToolCallAbandon::Abandoned
        })
    }

    /// Settle a session's unapplied asks before a turn builds on its
    /// transcript. Pending dead asks are abandoned (and stamped applied);
    /// resolved resumable ones come back as [`Settled::Apply`]; resolved
    /// non-resumable ones are stamped applied and reported as nothing. A live
    /// ask cannot exist here — it is logged and left alone.
    pub async fn settle_unapplied(&self, session_id: &SessionId) -> Result<Vec<Settled>> {
        let mut settled = Vec::new();
        for mut row in self.unapplied(session_id).await? {
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
    /// re-enqueued as dead entries (publishing `Raised`); resolved resumable
    /// ones are handed to the resume sink without being shown again;
    /// non-resumable ones are abandoned or stamped applied. Publishes queue
    /// edges only — never observer events. Idempotent.
    pub async fn reload_pending(&self) -> Result<ReloadReport> {
        let mut report = ReloadReport::default();
        for row in self.core.store().list_unapplied().await? {
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
                    let now = Utc::now();
                    let store = self.core.store();
                    if store
                        .resolve(&row.ask_id, &AskResolution::Abandoned, now)
                        .await?
                    {
                        report.abandoned += 1;
                    }
                    store.mark_applied(&row.ask_id, now).await?;
                }
                (false, true) => {
                    if self.request_resume(&row).await {
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
            .unapplied(session_id)
            .await?
            .into_iter()
            .filter(|row| resumable(row.kind, row.phase))
            .map(|row| row.tool_call_id)
            .collect())
    }

    /// The session's asks not yet applied to its transcript, live or dead.
    pub async fn unapplied_for_session(&self, session_id: &SessionId) -> Result<Vec<HumanAskRow>> {
        self.unapplied(session_id).await
    }

    pub fn has_live_pending_ask(&self, session_id: &SessionId) -> bool {
        self.queues
            .all()
            .iter()
            .any(|queue| queue.has_live(session_id))
    }

    async fn unapplied(&self, session_id: &SessionId) -> Result<Vec<HumanAskRow>> {
        Ok(self
            .core
            .store()
            .list_unapplied_for_session(session_id)
            .await?)
    }

    fn is_live(&self, row: &HumanAskRow) -> bool {
        self.queues
            .get(&row.channel)
            .and_then(|queue| queue.liveness(&row.ask_id))
            .unwrap_or(false)
    }

    /// Claim `resolution` on a pending row and mirror it onto `row`.
    async fn claim(&self, row: &mut HumanAskRow, resolution: AskResolution) -> Result<bool> {
        let now = Utc::now();
        let won = self
            .core
            .store()
            .resolve(&row.ask_id, &resolution, now)
            .await?;
        if won {
            row.resolved_at = Some(now);
            row.resolution = Some(resolution);
        }
        Ok(won)
    }

    async fn abandon_row(&self, row: &mut HumanAskRow) -> Result<bool> {
        if !self.claim(row, AskResolution::Abandoned).await? {
            return Ok(false);
        }
        self.core
            .notify(row, AskEvent::Resolved(AskResolution::Abandoned))
            .await;
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
    fn has_live_pending_ask(&self, session_id: &SessionId) -> bool {
        HumanAskManager::has_live_pending_ask(self, session_id)
    }
}

/// Whether `resolution` is something an ask of `kind` can resolve to.
fn fits(kind: AskKind, resolution: &AskResolution) -> bool {
    match resolution {
        AskResolution::Approval(_) => kind == AskKind::Approval,
        AskResolution::Answered(_) | AskResolution::Declined => kind == AskKind::Question,
        AskResolution::Abandoned | AskResolution::PolicyDenied => true,
    }
}
