//! The two [`ApprovalGate`] implementations backed by the ledger.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use baybo_model::human_ask::{AskId, AskKind, AskRequest, AskResolution, resumable};
use baybo_store::HumanAskRow;
use baybo_tools::{ApprovalGate, ApprovalOutcome, AskCancel, AskSite, CancelOrigin};
use chrono::Utc;
use tokio::sync::oneshot;

use crate::core::{AskCore, AskEvent};
use crate::queue::{AskQueue, PendingAsk};

/// How long a non-resumable ask — a during-execute approval — waits for an
/// answer. It cannot outlive its tool future, so without a deadline it would
/// hold its turn in memory for as long as nobody answers; when the deadline
/// passes the tool gets a timed-out denial and carries on. Resumable asks
/// have no deadline. See `docs/todo/during-execute-approvals.md`.
pub const DURING_EXECUTE_APPROVAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A channel's gate: persist the ask, enqueue it, wait for a verb to resolve
/// it — with no timeout, except a non-resumable ask's
/// [`DURING_EXECUTE_APPROVAL_TIMEOUT`]. Clients learn of the ask from the queue's
/// [`AskQueueEvent::AskRaised`](crate::AskQueueEvent::AskRaised), which the
/// channel's ask watcher turns into a frame.
pub struct ChannelAskGate {
    queue: AskQueue,
    core: Arc<AskCore>,
}

impl ChannelAskGate {
    pub fn new(queue: AskQueue, core: Arc<AskCore>) -> Self {
        Self { queue, core }
    }

    pub fn queue(&self) -> &AskQueue {
        &self.queue
    }

    /// Persist, enqueue and wait. `None` when the row could not be
    /// written — nothing was shown, so there is nothing to wait for.
    ///
    /// On a turn cancel the arm returns at once without awaiting anything:
    /// the loop drops the tool future the moment this yields `Pending`, so a
    /// write started there would be torn. The write-ahead insert itself can
    /// outlive a dropped future and commit after the loop's cancel fill; the
    /// settlement skips a call that already has a result. On an ancestor cancel (process
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
        self.queue.push_live(ask, tx);
        let _orphan = OrphanOnDrop {
            queue: self.queue.clone(),
            ask_id: row.ask_id.clone(),
        };

        let deadline = async {
            if resumable(row.kind, row.phase) {
                std::future::pending::<()>().await;
            } else {
                tokio::time::sleep(DURING_EXECUTE_APPROVAL_TIMEOUT).await;
            }
        };
        let mut rx = rx;
        let resolution = tokio::select! {
            biased;
            answered = &mut rx => answered.unwrap_or(AskResolution::Abandoned),
            _ = cancel.tokens.turn.cancelled() => {
                return match cancel.origin() {
                    CancelOrigin::Turn => Some(AskResolution::Abandoned),
                    CancelOrigin::Ancestor => std::future::pending().await,
                };
            }
            () = deadline => self.expire(&row, rx).await,
        };
        if let Err(e) = self.core.mark_applied(&row.ask_id).await {
            tracing::warn!(ask_id = %row.ask_id, error = %e, "could not stamp ask applied");
        }
        Some(resolution)
    }

    /// The deadline passed: claim `TimedOut` on the still-pending row and
    /// close the card. A verb that claimed first wins, and its answer is
    /// already on its way down `rx`. If the claim cannot be written the ask
    /// still times out here; the row stays pending and is reconciled as a
    /// dead non-resumable ask.
    async fn expire(
        &self,
        row: &HumanAskRow,
        rx: oneshot::Receiver<AskResolution>,
    ) -> AskResolution {
        let timed_out = AskResolution::TimedOut;
        match self
            .core
            .store()
            .resolve(&row.ask_id, &timed_out, Utc::now())
            .await
        {
            Ok(true) => {
                self.core
                    .notify(row, AskEvent::Resolved(timed_out.clone()))
                    .await;
            }
            Ok(false) => return rx.await.unwrap_or(AskResolution::Abandoned),
            Err(e) => {
                tracing::warn!(ask_id = %row.ask_id, error = %e, "could not record an ask timing out");
            }
        }
        tracing::info!(ask_id = %row.ask_id, "during-execute approval timed out");
        let _ = self.queue.resolve(&row.ask_id, timed_out.clone());
        timed_out
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
        let row = approval_row(req, site);
        let ask_id = row.ask_id.clone();
        match self.park(row, cancel).await {
            Some(AskResolution::Approval(decision)) => ApprovalOutcome::answered(decision),
            Some(AskResolution::PolicyDenied) => ApprovalOutcome::policy(ApprovalDecision::Deny),
            Some(AskResolution::Abandoned) | None => ApprovalOutcome::abandoned(),
            Some(AskResolution::TimedOut) => ApprovalOutcome::timed_out(),
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
        let mut row = approval_row(req, site);
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

/// The pending ledger row for an approval.
fn approval_row(req: ApprovalRequest, site: AskSite) -> HumanAskRow {
    HumanAskRow {
        ask_id: req.ask_id.clone(),
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
    }
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
