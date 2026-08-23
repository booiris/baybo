//! Stopping a run that is already executing.
//!
//! The board records and settles runs; it does not own the turns underneath
//! them. The seam is kept even though this crate can see `baybo-turn`: what
//! [`ProjectManager`](crate::ProjectManager) holds is the verb, never the
//! lifecycle, so no rule about turns can be answered from inside the board
//! and no test needs a turn store to build one.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::SessionId;
use baybo_turn::{CancelReason, TurnLifecycle};

/// Why the board is stopping a run that is already executing.
///
/// The board's own vocabulary, not the turn layer's: the implementation
/// maps it onto whatever the turn layer calls these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStopReason {
    /// Somebody pressed stop.
    Operator,
    /// The board crossed its money ceiling with this run still working.
    BudgetExhausted,
}

/// Stops the turn under a card's live run.
///
/// The executor watching that turn is what settles the ledger row, so this
/// only has to interrupt: implementations must not write run state.
#[async_trait]
pub trait IssueRunStopper: Send + Sync {
    /// Stop whatever is executing in `session`. Idempotent: a session with
    /// nothing live is not an error, because the board re-reads a stopped
    /// run on its next tick before its executor has settled it.
    async fn stop_run(&self, session: &SessionId, reason: RunStopReason) -> Result<(), String>;
}

/// Stops an issue's live run by cancelling the turns under its session.
///
/// Listing the session's *active* turns first is what makes it idempotent:
/// a run the board asks to stop twice — the cancel is asynchronous, so the
/// row is still `Running` on the next tick — finds nothing live the second
/// time.
struct TurnRunStopper {
    turns: Arc<TurnLifecycle>,
}

#[async_trait]
impl IssueRunStopper for TurnRunStopper {
    async fn stop_run(&self, session: &SessionId, reason: RunStopReason) -> Result<(), String> {
        let reason = match reason {
            RunStopReason::Operator => CancelReason::OperatorCancel,
            RunStopReason::BudgetExhausted => CancelReason::BudgetExhausted,
        };
        let turns = self
            .turns
            .list_active_chat_turns_by_session(session)
            .await
            .map_err(|e| format!("issue run turns: {e}"))?;
        for turn in turns {
            self.turns
                .cancel(&turn.id, reason, vec![])
                .await
                .map_err(|e| format!("cancel issue run: {e}"))?;
        }
        Ok(())
    }
}

/// The stopper a real assembly wants: cancel the turn, and let the executor
/// watching it settle the ledger row.
pub fn turn_run_stopper(turns: Arc<TurnLifecycle>) -> Arc<dyn IssueRunStopper> {
    Arc::new(TurnRunStopper { turns })
}
