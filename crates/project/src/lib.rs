//! Kanban projects: the container, its board, and the rules a write has
//! to satisfy before it reaches the store.
//!
//! See `docs/todo/kanban.md`. This crate owns validation and workdir
//! materialisation; `baybo-store` declares the persistence port and
//! `baybo-storage` implements it.

mod error;
mod manager;

pub use error::{ProjectError, Result};
pub use manager::{
    MAX_ISSUE_TITLE_CHARS, NewIssueRequest, NewProject, ProjectManager, validate_workdir,
};
