//! Kanban projects: the container, its board, and the rules a write has
//! to satisfy before it reaches the store.

mod approvals;
mod budget;
mod comments;
mod error;
mod events;
mod manager;
mod mentions;
mod runs;
mod stages;
mod timeline;
pub mod tools;
pub mod worktree;

pub use approvals::TimelineApprovalGate;
pub use comments::CommentDelivery;
pub use error::{ProjectError, Result};
pub use events::{NoopProjectEvents, ProjectEvents};
pub use manager::{
    LEAD_HANDLE, MAX_FEED_PAGE, MAX_TEAM_AGENTS, NewIssueRequest, NewProject, NewTeamMember,
    ProjectManager, RunDispatch, no_dispatch, validate_workdir,
};
pub use runs::can_host_a_session;
pub use stages::progress;
pub use worktree::Checkout;
