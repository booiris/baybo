//! Kanban projects: the container, its board, and the rules a write has
//! to satisfy before it reaches the store.

mod actors;
mod approvals;
mod artifacts;
mod attachments;
mod brief;
mod budget;
mod comments;
pub mod dispatch;
mod driver;
mod error;
mod events;
mod manager;
mod mentions;
mod runs;
mod settle;
mod stages;
mod stopper;
mod timeline;
pub mod tools;
pub mod worktree;

pub use approvals::{CardPromptCloser, TimelineApprovalGate};
pub use artifacts::{BuildArtifacts, ReclaimedArtifacts};
pub use attachments::{AttachmentRequest, MAX_ISSUE_ATTACHMENTS};
pub use comments::CommentDelivery;
pub use dispatch::{DispatchConfig, IssueRunEvent};
pub use error::{ProjectError, Result};
pub use events::{NoopProjectEvents, ProjectEvents};
pub use manager::{
    FeedEntry, IssueRunLog, LEAD_HANDLE, MAX_FEED_PAGE, MAX_TEAM_AGENTS, NewIssueRequest,
    NewProject, NewTeamMember, Placement, ProjectManager, RunDispatch, RunWithSpend, no_dispatch,
    no_stopper, validate_workdir,
};
/// Reachable only from a test build. The agent crate's tests assert that
/// the session its router opens and the window this crate's brief is a
/// delta from name the same run — an invariant that spans both crates, so
/// neither can check it alone.
#[cfg(any(test, feature = "test-support"))]
pub use runs::session_run_before;
pub use runs::{RunOutcome, can_host_a_session, session_run_to_continue};
pub use stages::progress;
pub use stopper::{IssueRunStopper, RunStopReason, turn_run_stopper};
pub use worktree::{Checkout, ProjectRepo};
