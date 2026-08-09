//! Ending a run, and what the board is owed when one ends.
//!
//! Every settle goes through here, whoever asked for it: the executor whose
//! turn finished, an operator cancelling a queued run, the boot sweep
//! calling one off on a card somebody has since closed, or a dispatcher
//! that could not cut a checkout. Settling is three writes, not one — the
//! ledger row, the run's own invalidation, and the timeline entry saying
//! how it ended — and a caller that does only the first leaves a card
//! saying something the ledger under it contradicts.
//!
//! Nothing here consults [`writable_project`](crate::ProjectManager), and
//! that is deliberate: a run archived out from under mid-flight still has
//! to be able to finish its own bookkeeping. Refusing the write would
//! strand the card, which is the failure the settle exists to prevent.

use std::sync::Arc;

use baybo_store::project::{
    IssueActor, IssueEventBody, IssueRunRow, NewIssueEvent, ProjectStore, RunStatus,
};

use crate::error::Result;
use crate::events::ProjectEvents;

/// Settle a run and tell the board. `Ok(false)` means somebody else settled
/// it first, which is the same outcome and owes the board nothing more.
pub(crate) async fn settle_run(
    store: &Arc<dyn ProjectStore>,
    events: &Arc<dyn ProjectEvents>,
    run: &IssueRunRow,
    actor: IssueActor,
    status: RunStatus,
    error: Option<&str>,
) -> Result<bool> {
    if !store.settle_run(&run.id, status, error).await? {
        return Ok(false);
    }
    events.run_changed(&run.project_id, run.number);
    record(
        store,
        events,
        run,
        actor,
        IssueEventBody::RunSettled {
            run_id: run.id.clone(),
            attempt: run.attempt,
            status,
            error: error.map(str::to_owned),
        },
    )
    .await;
    Ok(true)
}

/// Append one entry addressed by the run that caused it.
///
/// Built from the run row rather than re-read by number: the row carries
/// every field the entry needs, and a run's own bookkeeping must not depend
/// on its card still being readable.
pub(crate) async fn record(
    store: &Arc<dyn ProjectStore>,
    events: &Arc<dyn ProjectEvents>,
    run: &IssueRunRow,
    actor: IssueActor,
    body: IssueEventBody,
) {
    let entry = NewIssueEvent {
        issue_id: run.issue_id.clone(),
        project_id: run.project_id.clone(),
        number: run.number,
        actor,
        body,
    };
    match store.append_event(&entry).await {
        Ok(_) => events.timeline_changed(&run.project_id, run.number),
        Err(e) => {
            tracing::error!(run = %run.id, error = %e, "could not record a run's timeline entry");
        }
    }
}
