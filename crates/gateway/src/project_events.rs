//! Gateway-side [`baybo_project::ProjectEvents`] implementation: board
//! invalidations ride the owner channel like every other live signal.

use std::sync::Arc;

use baybo_channels::ChannelRegistry;
use baybo_channels::wire::ProjectChangeScope;
use baybo_model::{ChannelType, ProjectId};

/// Broadcasts board frames to every owner-channel connection. Clients
/// without board UI ignore them by contract (`crates/wire`).
pub struct GatewayProjectEvents {
    registry: Arc<ChannelRegistry>,
}

impl GatewayProjectEvents {
    pub fn new(registry: Arc<ChannelRegistry>) -> Self {
        Self { registry }
    }

    fn emit(&self, project: &ProjectId, scope: ProjectChangeScope, issue: Option<i64>) {
        // The wire field is u32 (TS BigInt avoidance); an issue number is a
        // per-project sequence that will never come close.
        let issue = match issue.map(u32::try_from) {
            Some(Ok(number)) => Some(number),
            Some(Err(_)) => {
                tracing::warn!(%project, "project: issue number exceeds wire range; dropping push");
                return;
            }
            None => None,
        };
        if let Some(channel) = self.registry.get(&ChannelType::owner())
            && let Some(sub) = channel.as_subscribed()
        {
            sub.broadcast_project_changed(project.as_str().to_owned(), scope, issue);
        }
    }
}

impl baybo_project::ProjectEvents for GatewayProjectEvents {
    fn project_changed(&self, project: &ProjectId) {
        self.emit(project, ProjectChangeScope::Project, None);
    }

    fn board_changed(&self, project: &ProjectId, issue: Option<i64>) {
        self.emit(project, ProjectChangeScope::Board, issue);
    }

    fn run_changed(&self, project: &ProjectId, issue: i64) {
        self.emit(project, ProjectChangeScope::Run, Some(issue));
    }

    fn timeline_changed(&self, project: &ProjectId, issue: i64) {
        self.emit(project, ProjectChangeScope::Timeline, Some(issue));
    }
}
