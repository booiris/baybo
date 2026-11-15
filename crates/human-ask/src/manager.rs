//! [`HumanAskManager`]: the verbs every other crate calls to move an ask out
//! of a state. Each verb persists through a claim first and only then
//! touches memory; the claim is the arbiter between concurrent verbs.

use std::sync::Arc;

use baybo_model::SessionId;
use baybo_model::human_ask::{AskId, AskKind, AskResolution};
use baybo_store::HumanAskRow;
use chrono::Utc;

use crate::core::{AskCore, AskEvent};
use crate::error::{HumanAskError, Result};
use crate::ports::LivePendingAsks;
use crate::queue::AskQueues;

pub struct HumanAskManagerConfig {
    pub core: Arc<AskCore>,
    /// The same queues the channels fetch from.
    pub queues: AskQueues,
}

pub struct HumanAskManager {
    core: Arc<AskCore>,
    queues: AskQueues,
}

impl HumanAskManager {
    pub fn from_config(config: HumanAskManagerConfig) -> Self {
        let HumanAskManagerConfig { core, queues } = config;
        Self { core, queues }
    }

    pub async fn get(&self, ask_id: &AskId) -> Result<Option<HumanAskRow>> {
        Ok(self.core.store().get(ask_id).await?)
    }

    /// A human (or policy) decided `ask_id`. Claims the resolution, then
    /// removes the queue entry and hands the decision to the live waiter.
    /// `Err(AskClosed)` when another verb claimed it first; `Err(NotLive)`
    /// when no tool future in this process waits on it — nothing could apply
    /// the answer, so the row is left for whoever closes it (the turn's
    /// cancel claim, or the next boot).
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
        if row.is_pending() && !self.is_live(&row) {
            return Err(HumanAskError::NotLive {
                ask_id: ask_id.clone(),
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
            None => false,
        };
        if !delivered {
            // The waiter went away between the liveness check and the claim.
            // Its turn's cancel claim, or the next boot, stamps the row.
            tracing::warn!(%ask_id, "resolved an ask whose waiter is gone");
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
            .unapplied(session_id)
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

    /// Boot, before any channel starts and before any tool future can park:
    /// close every unapplied ask a previous process left behind. Pending
    /// ones are claimed `Abandoned` (observers see it); every one is stamped
    /// applied, so nothing later reads it as an answer still owed. The
    /// dangling `tool_use` gets hydration's interrupted fill. Returns how
    /// many asks this call abandoned.
    pub async fn close_stale_asks(&self) -> Result<usize> {
        let mut abandoned = 0;
        for mut row in self.core.store().list_unapplied().await? {
            if row.is_pending() && self.abandon_row(&mut row).await? {
                abandoned += 1;
            }
            self.core.mark_applied(&row.ask_id).await?;
        }
        Ok(abandoned)
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
