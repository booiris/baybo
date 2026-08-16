//! Turning a recorded run into something executable.
//!
//! [`ProjectManager`](crate::ProjectManager) writes the ledger row and hands
//! it to a [`RunDispatch`](crate::RunDispatch); what that dispatcher owes the
//! executor is here. See `docs/modules/project.md` for the race a dispatcher
//! shares with the boot sweep — the checkout is cut before the claim, so two
//! dispatchers on one row both reach `worktree::ensure`.

use std::path::PathBuf;
use std::sync::Arc;

use baybo_model::{ChannelType, MediaBlock};
use baybo_store::project::{IssueActor, IssueRunRow, ProjectStore, RunStatus};
use baybo_store::{AgentProfileStore, BlobStore};
use baybo_workspace::WorkspacePaths;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::brief::{comments_for_brief, issue_brief, issue_brief_media};
use crate::events::ProjectEvents;
use crate::manager::RunDispatch;
use crate::worktree;

/// What a run needs to execute, as the dispatcher hands it over.
#[derive(Debug, Clone)]
pub struct IssueRunEvent {
    pub run: IssueRunRow,
    /// The brief the assignee is asked to work on — the card and what has
    /// been said on it, assembled here so the executor never has to read
    /// the board.
    ///
    pub brief: String,
    /// The card's files, as the blocks a model is handed — so a run sees a
    /// mockup rather than reading its filename. Kept apart from
    /// [`brief`](Self::brief) because the prompt framing wraps the prose
    /// and nothing else, and typed as media so nothing else can be put here.
    pub files: Vec<MediaBlock>,
    /// The instant [`brief`](Self::brief) was read off the card.
    ///
    /// The boundary between what this run was told and what it was not, and
    /// the only honest one: the claim comes later — after a `git worktree
    /// add` and a trip through the run channel — and everything said in
    /// between is in neither the brief nor a window bounded by the claim.
    /// See `ProjectManager::finish_run`, which is where it is spent.
    pub briefed_at: DateTime<Utc>,
    /// The worktree this run works in, already cut by the dispatcher.
    pub checkout: PathBuf,
    /// Who the run belongs to, for session ownership.
    pub user_id: String,
    pub channel: ChannelType,
}

/// Everything a dispatcher needs that it cannot derive. The identity is
/// injected rather than read here: who owns a run is the assembling
/// process's answer, not the board's.
pub struct DispatchConfig {
    pub store: Arc<dyn ProjectStore>,
    /// How an agent id becomes the `@handle` the brief attributes a comment
    /// to. The board's own store: an agent on another project is named by
    /// its id rather than by a handle this board never issued.
    pub agents: Arc<dyn AgentProfileStore>,
    /// Where a card's files live. The brief resolves them here rather than
    /// naming blob ids the executor would have to spend.
    pub blobs: Arc<dyn BlobStore>,
    /// The same hook the manager announces through. A dispatcher settles
    /// the runs it cannot prepare, and a settle nobody hears leaves the
    /// card shimmering — which is the outcome settling here exists to
    /// prevent.
    pub events: Arc<dyn ProjectEvents>,
    pub paths: WorkspacePaths,
    pub user_id: String,
    pub channel: ChannelType,
}

/// How many prepared runs may wait on the executor before a dispatcher
/// blocks. Each is one card in the In Progress column.
const RUN_QUEUE_DEPTH: usize = 64;

/// Build the dispatcher and the receiver its runs come out of. Every run is
/// prepared on its own task, so one slow checkout does not hold up the
/// board write that produced the next.
pub fn build(config: DispatchConfig) -> (RunDispatch, mpsc::Receiver<IssueRunEvent>) {
    let (tx, rx) = mpsc::channel(RUN_QUEUE_DEPTH);
    // One `Arc` for the whole config rather than one per field: every run
    // clones this into its own task, and the alternative grows another clone
    // line each time a dispatcher learns to look something up.
    let config = Arc::new(config);

    let dispatch = Arc::new(move |run: IssueRunRow| {
        let config = Arc::clone(&config);
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(event) = prepare(&config, run).await
                && let Err(e) = tx.send(event).await
            {
                tracing::warn!(error = %e, "issue run could not reach its executor");
            }
        });
    }) as RunDispatch;

    (dispatch, rx)
}

async fn prepare(config: &DispatchConfig, run: IssueRunRow) -> Option<IssueRunEvent> {
    let DispatchConfig {
        store,
        agents,
        blobs,
        events,
        paths,
        user_id,
        channel,
    } = config;
    // The brief is read at dispatch, not at enqueue: a comment left while
    // the run was queued is part of what it should work on.
    let issue = match store.get_issue(&run.project_id, run.number).await {
        Ok(Some(issue)) => issue,
        Ok(None) => {
            tracing::warn!(run = %run.id, "issue is gone; not running it");
            return None;
        }
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read issue for run");
            return None;
        }
    };
    // Stamped before the read, not after: a comment landing mid-read is one
    // the brief may not have caught, and the follow-up window has to be the
    // side that over-reads rather than the side that drops it.
    let briefed_at = Utc::now();
    let said = comments_for_brief(store, agents, &run).await;
    let brief = issue_brief(&issue, &said, run.trigger);
    let files = issue_brief_media(blobs, &issue, &said).await;

    let checkout = match worktree::prepare_for_issue(store, paths, &issue).await {
        Ok(root) => root,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not open the issue's checkout");
            // Settle here too: nothing downstream will ever see this run, so
            // nothing downstream can settle it, and a card that shimmers
            // forever is the alternative. Through the same path as every
            // other settle, so the board hears it and the timeline says why.
            if let Err(settling) = crate::settle::settle_run(
                store,
                events,
                // No checkout means no tool could have parked a prompt.
                None,
                &run,
                IssueActor::System,
                RunStatus::Failed,
                Some(&e.to_string()),
            )
            .await
            {
                tracing::error!(run = %run.id, error = %settling, "could not settle a run whose checkout failed");
            }
            return None;
        }
    };

    Some(IssueRunEvent {
        run,
        brief,
        files,
        briefed_at,
        checkout,
        user_id: user_id.clone(),
        channel: channel.clone(),
    })
}
