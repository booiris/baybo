//! Kanban projects: the container, its board, and the rules a write has
//! to satisfy before it reaches the store.
//!
//! See `docs/modules/project.md` for the architecture, and
//! `docs/todo/kanban.md` for the product rationale and phasing. This crate
//! owns the board's write rules, the run ledger's single enqueue path, and
//! the per-issue worktree; `baybo-store` declares the persistence port,
//! `baybo-storage` implements it, `baybo-agent` executes the runs recorded
//! here, and `baybo-gateway` serves the board.

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

// Only what a downstream crate actually consumes. Everything else these
// modules expose is `pub(crate)` and reached as `crate::…`: an item
// re-exported here is a promise to the gateway, the runtime and this
// crate's own tests, and a promise nobody is holding is one that quietly
// grows callers it was never designed for.
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
