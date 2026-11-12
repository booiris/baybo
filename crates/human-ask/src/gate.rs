//! The gates that answer an [`ApprovalGate`] request: [`ChannelAskGate`]
//! parks the call on its channel's [`AskQueue`] until a human answers;
//! [`PolicyDenyGate`] denies without asking anyone.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use baybo_tools::{ApprovalGate, ApprovalOutcome};
use tokio::sync::oneshot;

use crate::queue::{AskQueue, PendingApproval};

/// Generic [`ApprovalGate`] for any channel. Pushes requests onto an
/// [`AskQueue`] and fires a sync waker callback so the channel's event
/// loop redraws. The channel resolves entries by calling
/// [`AskQueue::resolve_head`].
///
/// Fail-closed: if the oneshot is dropped without a decision (e.g. the
/// channel exits while approvals are pending) or the timeout expires,
/// `Deny` is returned.
pub struct ChannelAskGate {
    queue: AskQueue,
    /// Handed the request it is being woken FOR. It must not re-read the queue
    /// to find one: the queue is per-CHANNEL, so two sessions prompting at once
    /// race, and a waker that took (say) the tail would broadcast one prompt
    /// twice and the other never — leaving a user staring at a turn that
    /// silently denies itself minutes later.
    waker: Arc<dyn Fn(ApprovalRequest) + Send + Sync>,
    timeout: Duration,
}

impl ChannelAskGate {
    pub fn new(
        queue: AskQueue,
        waker: Arc<dyn Fn(ApprovalRequest) + Send + Sync>,
        timeout: Duration,
    ) -> Self {
        Self {
            queue,
            waker,
            timeout,
        }
    }

    /// Access the underlying queue so the channel can peek/resolve entries.
    pub fn queue(&self) -> &AskQueue {
        &self.queue
    }
}

#[async_trait]
impl ApprovalGate for ChannelAskGate {
    async fn request(&self, req: ApprovalRequest) -> ApprovalOutcome {
        let call_id = req.call_id.clone();
        let (tx, rx) = oneshot::channel();
        let woken_for = req.clone();
        self.queue.push(PendingApproval {
            req,
            responder: Some(tx),
        });
        (self.waker)(woken_for);

        // RAII guard: if our future is dropped before a decision lands
        // (e.g. the outer tool timeout fires while the user is still
        // reading the modal) or the gate's own timeout elapses, remove
        // the queue entry so the channel UI doesn't catch the user's
        // *next* click on a stale, disconnected `tx` — which would
        // cost an extra click per orphan and could let a stale
        // resolution silently land on a disconnected receiver.
        let _cleanup = QueueCleanup {
            queue: self.queue.clone(),
            call_id,
        };

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => ApprovalOutcome::answered(decision),
            // The responder is gone with no decision: the queue entry was
            // torn down under us (turn cancel, channel exit).
            Ok(Err(_)) => ApprovalOutcome::abandoned(),
            Err(_) => ApprovalOutcome::timed_out(),
        }
    }
}

/// RAII helper that removes a queue entry when its owning request
/// future is dropped. A no-op if `resolve_head` / `resolve_by_call_id`
/// already popped the entry (the matching `call_id` is gone, and
/// `drop_call` returns `false`).
struct QueueCleanup {
    queue: AskQueue,
    call_id: String,
}

impl Drop for QueueCleanup {
    fn drop(&mut self) {
        let _ = self.queue.drop_call(&self.call_id);
    }
}

/// The gate every ask of an unattended turn goes to, in both phases: nobody
/// can see the prompt, so it is denied by policy without being raised.
/// Fail-closed for the same reason as `baybo_tools::AutoDenyGate`, but kept
/// distinct: that one guards a channel with no gate registered, this one is
/// the attendedness rule's answer.
#[derive(Debug, Default)]
pub struct PolicyDenyGate;

impl PolicyDenyGate {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ApprovalGate for PolicyDenyGate {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
        ApprovalOutcome::policy(ApprovalDecision::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::approval::ApprovalResolution;

    #[tokio::test]
    async fn policy_deny_gate_denies_by_policy() {
        let out = PolicyDenyGate::new()
            .request(ApprovalRequest {
                call_id: "x".into(),
                tool_call_id: "use-1".into(),
                session_id: "s".into(),
                user_id: "u".into(),
                tool: "Bash".into(),
                accesses: vec![],
                params_preview: String::new(),
                description: None,
            })
            .await;
        assert_eq!(out.decision, ApprovalDecision::Deny);
        assert_eq!(out.resolution, ApprovalResolution::Policy);
    }
}
