//! Board push hooks.
//!
//! The port lives here so this crate never depends on the gateway or on
//! the wire types: it speaks issue numbers and project ids, and the
//! gateway turns those into a frame on the owner channel.

use baybo_model::ProjectId;

pub trait ProjectEvents: Send + Sync + 'static {
    /// The project row changed — renamed, re-described, archived.
    fn project_changed(&self, project: &ProjectId);

    /// The board changed — an issue was created, edited, moved, blocked or
    /// cancelled. `issue` names it when the change is about a single one.
    fn board_changed(&self, project: &ProjectId, issue: Option<i64>);

    /// One issue's run state advanced — queued, started, settled.
    fn run_changed(&self, project: &ProjectId, issue: i64);

    /// One issue gained a timeline entry — a comment, or a system note.
    /// Separate from [`Self::board_changed`] because it moves no card: a
    /// board watching for it would refetch every column to learn that
    /// somebody said something on one.
    fn timeline_changed(&self, project: &ProjectId, issue: i64);
}

/// A hook that announces nothing. What a headless assembly and every
/// store-level test wants — production-legitimate, not a test double.
pub struct NoopProjectEvents;

impl ProjectEvents for NoopProjectEvents {
    fn project_changed(&self, _project: &ProjectId) {}
    fn board_changed(&self, _project: &ProjectId, _issue: Option<i64>) {}
    fn run_changed(&self, _project: &ProjectId, _issue: i64) {}
    fn timeline_changed(&self, _project: &ProjectId, _issue: i64) {}
}
