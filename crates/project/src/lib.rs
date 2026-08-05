//! Kanban projects: the container, its board, and the rules a write has
//! to satisfy before it reaches the store.
//!
//! See `docs/todo/kanban.md`. This crate owns validation and workdir
//! materialisation; `baybo-store` declares the persistence port and
//! `baybo-storage` implements it.

mod comments;
mod error;
mod events;
mod manager;
mod runs;
mod timeline;
pub mod worktree;

pub use comments::{CommentDelivery, comment_delivery};
pub use error::{ProjectError, Result};
pub use events::{NoopProjectEvents, ProjectEvents};
pub use manager::{
    LEAD_HANDLE, MAX_ISSUE_TITLE_CHARS, MAX_ROLE_CHARS, MAX_TEAM_AGENTS, NewIssueRequest,
    NewProject, NewTeamMember, ProjectManager, RunDispatch, no_dispatch, validate_workdir,
};
pub use runs::{Transition, triggers_run};
pub use timeline::diff_events;
pub use worktree::Checkout;
