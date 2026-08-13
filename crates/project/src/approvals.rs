//! Putting a run's approval prompts on the card that asked for them.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{ApprovalDecision, ApprovalResolution, ProjectId, SessionId};
use baybo_store::SessionStore;
use baybo_store::project::{IssueActor, IssueEventBody};
use baybo_tools::{ApprovalGate, ApprovalOutcome, ApprovalRequest};

use crate::ProjectManager;

const MAX_SUMMARY_CHARS: usize = 160;

/// Wraps a channel's approval gate and writes what it sees onto the issue's
/// timeline.
pub struct TimelineApprovalGate {
    /// The gate that actually prompts. Held directly rather than resolved
    /// per call, because resolving would find *this* wrapper.
    inner: Arc<dyn ApprovalGate>,
    manager: Arc<ProjectManager>,
    sessions: Arc<dyn SessionStore>,
}

impl TimelineApprovalGate {
    pub fn new(
        inner: Arc<dyn ApprovalGate>,
        manager: Arc<ProjectManager>,
        sessions: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            inner,
            manager,
            sessions,
        }
    }

    async fn card(&self, session_id: &SessionId) -> Option<(baybo_model::ProjectId, i64)> {
        let session = self.sessions.get(session_id).await.ok().flatten()?;
        session
            .trigger
            .issue()
            .map(|(project, _, number)| (project.clone(), number))
    }
}

#[async_trait]
impl ApprovalGate for TimelineApprovalGate {
    async fn request(&self, req: ApprovalRequest) -> ApprovalOutcome {
        let Some((project, number)) = self.card(&req.session_id).await else {
            return self.inner.request(req).await;
        };
        let call_id = req.call_id.clone();
        let actor = match self.manager.get_issue(&project, number).await {
            Ok(issue) => issue.assignee.map_or(IssueActor::User, IssueActor::Agent),
            Err(_) => IssueActor::User,
        };
        // Which run is parked. The dedupe guard keeps at most one run per
        // issue in flight, so "the unsettled one" is unambiguous — the same
        // reasoning the run waiter relies on.
        let attempt = self
            .manager
            .list_runs(&project, number)
            .await
            .ok()
            .and_then(|runs| {
                runs.into_iter()
                    .find(|run| run.settled_at.is_none())
                    .map(|run| run.attempt)
            });
        self.manager
            .record_event(
                &project,
                number,
                actor.clone(),
                IssueEventBody::ApprovalRequested {
                    call_id: call_id.clone(),
                    attempt,
                    tool: req.tool.clone(),
                    summary: summarise(&req),
                },
            )
            .await;

        // If this future is dropped while the inner gate is still parked —
        // the turn was cancelled, the process is shutting down — nothing
        // after the `.await` runs, and the request the card announced would
        // stay open on its timeline forever. The guard closes it as
        // abandoned instead; it is disarmed on the ordinary path below.
        let mut guard = AbandonGuard {
            manager: Arc::clone(&self.manager),
            project,
            number,
            actor,
            call_id: Some(call_id),
        };

        let outcome = self.inner.request(req).await;

        if let Some(call_id) = guard.disarm() {
            self.manager
                .record_event(
                    &guard.project,
                    guard.number,
                    guard.actor.clone(),
                    IssueEventBody::ApprovalResolved {
                        call_id,
                        decision: outcome.decision,
                        resolution: outcome.resolution,
                    },
                )
                .await;
        }
        outcome
    }
}

/// Closes an announced approval on the issue timeline when the request
/// future is dropped undecided. Best-effort by construction: `Drop` cannot
/// await, so the write is spawned, and a runtime already past its task
/// drain loses it — the failure mode is the status quo (an open request),
/// never a wrong entry.
struct AbandonGuard {
    manager: Arc<ProjectManager>,
    project: ProjectId,
    number: i64,
    actor: IssueActor,
    /// `Some` while armed; [`Self::disarm`] takes it so the ordinary
    /// resolution path writes its own event instead.
    call_id: Option<String>,
}

impl AbandonGuard {
    fn disarm(&mut self) -> Option<String> {
        self.call_id.take()
    }
}

impl Drop for AbandonGuard {
    fn drop(&mut self) {
        let Some(call_id) = self.call_id.take() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let manager = Arc::clone(&self.manager);
        let project = self.project.clone();
        let number = self.number;
        let actor = self.actor.clone();
        handle.spawn(async move {
            manager
                .record_event(
                    &project,
                    number,
                    actor,
                    IssueEventBody::ApprovalResolved {
                        call_id,
                        decision: ApprovalDecision::Deny,
                        resolution: ApprovalResolution::Abandoned,
                    },
                )
                .await;
        });
    }
}

fn summarise(req: &ApprovalRequest) -> String {
    let raw = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(&req.params_preview);
    let mut summary: String = raw.chars().take(MAX_SUMMARY_CHARS).collect();
    if raw.chars().count() > MAX_SUMMARY_CHARS {
        summary.push('…');
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_prefers_the_tools_own_label() {
        let base = ApprovalRequest {
            call_id: "c".into(),
            tool_call_id: None,
            session_id: SessionId::from("s".to_owned()),
            user_id: "u".into(),
            tool: "Bash".into(),
            accesses: Vec::new(),
            params_preview: "{\"command\":\"rm -rf build\"}".into(),
            description: Some("  Clean the build directory  ".into()),
        };
        assert_eq!(summarise(&base), "Clean the build directory");

        for description in [None, Some("   ".to_owned())] {
            let req = ApprovalRequest {
                description,
                ..base.clone()
            };
            assert_eq!(summarise(&req), "{\"command\":\"rm -rf build\"}");
        }

        let long = ApprovalRequest {
            description: Some("x".repeat(MAX_SUMMARY_CHARS + 10)),
            ..base
        };
        let summary = summarise(&long);
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }
}
