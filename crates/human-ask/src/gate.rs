//! The two [`ApprovalGate`] implementations backed by the ledger.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use baybo_model::human_ask::{AskId, AskKind, AskRequest, AskResolution};
use baybo_store::HumanAskRow;
use baybo_tools::{ApprovalGate, ApprovalOutcome, AskCancel, AskSite, CancelOrigin};
use chrono::Utc;
use tokio::sync::oneshot;

use crate::core::{AskCore, AskEvent};
use crate::queue::{AskQueue, PendingAsk};

/// Wakes the channel for the ask it was raised FOR. It must not re-read the
/// queue to find one: the queue is per channel, so two sessions asking at
/// once race, and a waker that took the tail would announce one ask twice
/// and the other never.
pub type Waker = Arc<dyn Fn(PendingAsk) + Send + Sync>;

/// A channel's gate: persist the ask, enqueue it, wake the channel, wait for
/// a verb to resolve it. No timeout.
pub struct ChannelAskGate {
    queue: AskQueue,
    waker: Waker,
    core: Arc<AskCore>,
}

impl ChannelAskGate {
    pub fn new(queue: AskQueue, waker: Waker, core: Arc<AskCore>) -> Self {
        Self { queue, waker, core }
    }

    pub fn queue(&self) -> &AskQueue {
        &self.queue
    }

    /// Persist, enqueue, wake and wait. `None` when the row could not be
    /// written — nothing was shown, so there is nothing to wait for.
    ///
    /// On a turn cancel the arm returns at once without awaiting anything:
    /// the loop drops the tool future the moment this yields `Pending`, so a
    /// write started there would be torn. On an ancestor cancel (process
    /// shutdown) it never returns, leaving the ask pending for the next
    /// process.
    async fn park(&self, row: HumanAskRow, cancel: AskCancel) -> Option<AskResolution> {
        if let Err(e) = self.core.store().create(&row).await {
            tracing::warn!(
                ask_id = %row.ask_id,
                kind = row.kind.as_str(),
                error = %e,
                "could not persist ask; not raising it"
            );
            return None;
        }
        self.core.notify(&row, AskEvent::Raised).await;

        let ask = PendingAsk::from_row(&row);
        let (tx, rx) = oneshot::channel();
        self.queue.push_live(ask.clone(), tx);
        let _orphan = OrphanOnDrop {
            queue: self.queue.clone(),
            ask_id: row.ask_id.clone(),
        };
        (self.waker)(ask);

        tokio::select! {
            biased;
            answered = rx => {
                let resolution = answered.unwrap_or(AskResolution::Abandoned);
                if let Err(e) = self.core.mark_applied(&row.ask_id).await {
                    tracing::warn!(ask_id = %row.ask_id, error = %e, "could not stamp ask applied");
                }
                Some(resolution)
            }
            _ = cancel.tokens.turn.cancelled() => match cancel.origin() {
                CancelOrigin::Turn => Some(AskResolution::Abandoned),
                CancelOrigin::Ancestor => std::future::pending().await,
            },
        }
    }
}

#[async_trait]
impl ApprovalGate for ChannelAskGate {
    async fn request(
        &self,
        req: ApprovalRequest,
        site: AskSite,
        cancel: AskCancel,
    ) -> ApprovalOutcome {
        let Some(row) = approval_row(req, site) else {
            return ApprovalOutcome::abandoned();
        };
        let ask_id = row.ask_id.clone();
        match self.park(row, cancel).await {
            Some(AskResolution::Approval(decision)) => ApprovalOutcome::answered(decision),
            Some(AskResolution::PolicyDenied) => ApprovalOutcome::policy(ApprovalDecision::Deny),
            Some(AskResolution::Abandoned) | None => ApprovalOutcome::abandoned(),
            Some(other @ (AskResolution::Answered(_) | AskResolution::Declined)) => {
                tracing::warn!(%ask_id, ?other, "approval ask resolved as a question; denying");
                ApprovalOutcome::abandoned()
            }
        }
    }
}

/// Unattended turns: every approval is denied by policy, and the denial is
/// still written to the ledger — already resolved and applied, with no queue
/// entry, edge or frame.
pub struct PolicyDenyGate {
    core: Arc<AskCore>,
}

impl PolicyDenyGate {
    pub fn new(core: Arc<AskCore>) -> Self {
        Self { core }
    }
}

#[async_trait]
impl ApprovalGate for PolicyDenyGate {
    async fn request(
        &self,
        req: ApprovalRequest,
        site: AskSite,
        _cancel: AskCancel,
    ) -> ApprovalOutcome {
        let denied = ApprovalOutcome::policy(ApprovalDecision::Deny);
        let Some(mut row) = approval_row(req, site) else {
            return denied;
        };
        let now = Utc::now();
        row.resolved_at = Some(now);
        row.resolution = Some(AskResolution::PolicyDenied);
        row.applied_at = Some(now);
        match self.core.store().create(&row).await {
            Ok(()) => {
                self.core
                    .notify(&row, AskEvent::Resolved(AskResolution::PolicyDenied))
                    .await;
            }
            Err(e) => {
                tracing::warn!(ask_id = %row.ask_id, error = %e, "could not record policy denial");
            }
        }
        denied
    }
}

/// The pending ledger row for an approval. `None` (logged) when the request
/// carries no usable ask id.
fn approval_row(req: ApprovalRequest, site: AskSite) -> Option<HumanAskRow> {
    let ask_id: AskId = match req.call_id.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(tool = %req.tool, error = %e, "approval request has no ask id");
            return None;
        }
    };
    Some(HumanAskRow {
        ask_id,
        session_id: req.session_id.clone(),
        channel: site.channel,
        tool_call_id: req.tool_call_id.clone(),
        kind: AskKind::Approval,
        phase: site.phase,
        request: AskRequest::Approval(req),
        created_at: Utc::now(),
        resolved_at: None,
        resolution: None,
        applied_at: None,
    })
}

/// A dropped waiter orphans its ask; it never resolves it.
struct OrphanOnDrop {
    queue: AskQueue,
    ask_id: AskId,
}

impl Drop for OrphanOnDrop {
    fn drop(&mut self) {
        self.queue.orphan(&self.ask_id);
    }
}
