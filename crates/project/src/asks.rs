//! Putting a run's approval prompts on the card that asked for them, and
//! closing them when the run settles.
//!
//! The ask ledger owns the prompts; this crate only observes them
//! ([`TimelineAskObserver`]) and abandons a settled run's leftovers through
//! the ask manager ([`AskCloser`]). Wired once by [`install_ask_bridge`].

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use baybo_human_ask::{AskCore, AskEvent, AskObserver, HumanAskManager};
use baybo_model::human_ask::{AskRequest, AskResolution};
use baybo_model::{ApprovalDecision, ApprovalResolution, ProjectId, SessionId};
use baybo_store::project::{IssueActor, IssueEventBody};
use baybo_store::{HumanAskRow, SessionStore};
use baybo_tools::ApprovalRequest;

use crate::ProjectManager;

const MAX_SUMMARY_CHARS: usize = 160;

/// Closes a settled run's parked prompts.
#[async_trait]
pub(crate) trait CardPromptCloser: Send + Sync {
    async fn close_run_prompts(&self, session_id: &SessionId);
}

/// Wire the board to the ask ledger: record every approval an issue session
/// raises or resolves on its card, and abandon a run's prompts when the run
/// settles. Call once, after both managers exist.
pub fn install_ask_bridge(
    core: &AskCore,
    project: &Arc<ProjectManager>,
    asks: Arc<HumanAskManager>,
    sessions: Arc<dyn SessionStore>,
) {
    core.add_observer(Arc::new(TimelineAskObserver {
        manager: Arc::downgrade(project),
        sessions,
    }));
    project.set_prompt_closer(Arc::new(AskCloser { asks }));
}

/// Records `approval_requested` / `approval_resolved` on the card of the
/// issue session that raised the ask. Holds the board weakly:
/// `ProjectManager → closer → HumanAskManager → AskCore → observers` would
/// otherwise be a strong cycle.
struct TimelineAskObserver {
    manager: Weak<ProjectManager>,
    sessions: Arc<dyn SessionStore>,
}

impl TimelineAskObserver {
    /// The card this ask was raised on, and **who raised it**.
    ///
    /// The actor is the session's own binding, not the card's assignee. A
    /// coordination run executes as the board's lead while the card stays on
    /// its assignee, so a prompt named off the card said "@dev-1 asked you to
    /// approve" for a call @lead's run made — and the binding is write-once,
    /// so it answers for the whole session where the card's assignee moves
    /// under it.
    async fn asker(&self, session_id: &SessionId) -> Option<(ProjectId, i64, IssueActor)> {
        let session = self.sessions.get(session_id).await.ok().flatten()?;
        let (project, _, number) = session.trigger.issue()?;
        let actor = session
            .state
            .agent_id
            .clone()
            .map_or(IssueActor::User, IssueActor::Agent);
        Some((project.clone(), number, actor))
    }
}

#[async_trait]
impl AskObserver for TimelineAskObserver {
    async fn on_event(&self, ask: &HumanAskRow, event: AskEvent) {
        let AskRequest::Approval(req) = &ask.request else {
            return;
        };
        let body = match &event {
            AskEvent::Raised => None,
            AskEvent::Resolved(resolution) => match timeline_resolution(resolution) {
                Some(resolved) => Some(resolved),
                None => return,
            },
        };
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let Some((project, number, actor)) = self.asker(&ask.session_id).await else {
            return;
        };
        let call_id = ask.ask_id.to_string();
        let body = match body {
            Some((decision, resolution)) => IssueEventBody::ApprovalResolved {
                call_id,
                decision,
                resolution,
            },
            // Which run is parked. The dedupe guard keeps at most one run per
            // issue in flight, so "the unsettled one" is unambiguous — the
            // same reasoning the run waiter relies on.
            None => IssueEventBody::ApprovalRequested {
                call_id,
                attempt: unsettled_attempt(&manager, &project, number).await,
                tool: req.tool.clone(),
                summary: summarise(req),
            },
        };
        manager.record_event(&project, number, actor, body).await;
    }
}

/// How a resolution reads on the card. `None` for a policy denial (never
/// raised on the card, so there is no prompt to close) and for question
/// payloads.
fn timeline_resolution(
    resolution: &AskResolution,
) -> Option<(ApprovalDecision, ApprovalResolution)> {
    match resolution {
        AskResolution::Approval(decision) => Some((*decision, ApprovalResolution::Answered)),
        AskResolution::Abandoned => Some((ApprovalDecision::Deny, ApprovalResolution::Abandoned)),
        AskResolution::PolicyDenied | AskResolution::Answered(_) | AskResolution::Declined => None,
    }
}

async fn unsettled_attempt(
    manager: &ProjectManager,
    project: &ProjectId,
    number: i64,
) -> Option<i64> {
    manager
        .list_runs(project, number)
        .await
        .ok()?
        .into_iter()
        .find(|run| run.settled_at.is_none())
        .map(|run| run.attempt)
}

/// A settled run's prompts have nobody left to apply them: abandon every
/// pending ask of its session, live or dead. A raced *Allow* loses the
/// ledger claim and comes back `AskClosed`.
struct AskCloser {
    asks: Arc<HumanAskManager>,
}

#[async_trait]
impl CardPromptCloser for AskCloser {
    async fn close_run_prompts(&self, session_id: &SessionId) {
        if let Err(e) = self.asks.abandon_for_session(session_id).await {
            tracing::warn!(session = %session_id, error = %e, "could not abandon a settled run's prompts");
        }
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
            tool_call_id: String::new(),
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

    #[test]
    fn a_policy_denial_never_reaches_the_card() {
        assert_eq!(timeline_resolution(&AskResolution::PolicyDenied), None);
        assert_eq!(
            timeline_resolution(&AskResolution::Abandoned),
            Some((ApprovalDecision::Deny, ApprovalResolution::Abandoned))
        );
        assert_eq!(
            timeline_resolution(&AskResolution::Approval(ApprovalDecision::Approve)),
            Some((ApprovalDecision::Approve, ApprovalResolution::Answered))
        );
    }
}
