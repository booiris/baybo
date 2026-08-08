//! Board push hooks.

use baybo_model::ProjectId;

pub trait ProjectEvents: Send + Sync + 'static {
    fn project_changed(&self, project: &ProjectId);

    fn board_changed(&self, project: &ProjectId, issue: Option<i64>);

    fn run_changed(&self, project: &ProjectId, issue: i64);

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
