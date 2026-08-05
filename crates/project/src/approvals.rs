//! Putting a run's approval prompts on the card that asked for them.
//!
//! An issue run blocks on the same approval gate every other session does,
//! and the prompt itself surfaces where that channel's UI is. What was
//! missing is the *board's* side of it: a card that stops moving because a
//! person has not answered a modal looks exactly like a card whose agent
//! has silently wedged.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{ApprovalDecision, SessionId};
use baybo_store::SessionStore;
use baybo_store::project::{IssueActor, IssueEventBody};
use baybo_tools::{ApprovalGate, ApprovalRequest};

use crate::ProjectManager;

/// Upper bound on the one-line summary written to the timeline. The full
/// parameters live in the trace; this is what a person reads on a card.
const MAX_SUMMARY_CHARS: usize = 160;

/// Wraps a channel's approval gate and writes what it sees onto the issue's
/// timeline.
///
/// Installed once, over the **type-level** gate of the channel a board's
/// runs use — not per run. There is nothing to arm or disarm, so there is
/// nothing to leak when a run dies in an unusual way, and a board opened
/// after boot is covered without anybody remembering to register it.
///
/// A prompt from an ordinary session passes straight through: the trigger
/// lookup says it belongs to no issue, and this adds one session read to a
/// path that is already blocking on a human.
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

    /// The card this prompt belongs to, if any.
    ///
    /// Reads the session's trigger rather than taking the issue as a
    /// parameter: the gate is shared by every session on the channel, and a
    /// wrapper that had to be told which issue it was serving would be a
    /// wrapper somebody has to keep in sync with the run lifecycle.
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
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        let Some((project, number)) = self.card(&req.session_id).await else {
            return self.inner.request(req).await;
        };
        let call_id = req.call_id.clone();
        // The actor is the agent whose tool call is blocked — the card's
        // assignee. A prompt is something an agent asked for, not something
        // the operator did, and attributing it to the operator would make
        // the timeline read as if they had asked themselves.
        let actor = match self.manager.get_issue(&project, number).await {
            Ok(issue) => issue.assignee.map_or(IssueActor::User, IssueActor::Agent),
            Err(_) => IssueActor::User,
        };
        self.manager
            .record_event(
                &project,
                number,
                actor.clone(),
                IssueEventBody::ApprovalRequested {
                    call_id: call_id.clone(),
                    tool: req.tool.clone(),
                    summary: summarise(&req),
                },
            )
            .await;

        let decision = self.inner.request(req).await;

        // Written on every path, including the gate's own deny-on-timeout:
        // a card that stops explaining itself at the prompt is the worst
        // version of this feature, because the prompt is exactly where a
        // reader would go looking.
        self.manager
            .record_event(
                &project,
                number,
                actor,
                IssueEventBody::ApprovalResolved { call_id, decision },
            )
            .await;
        decision
    }
}

/// One line a person can decide from.
///
/// Prefers the tool's own label (Bash's `description`, WebFetch's URL) and
/// falls back to the parameter preview, which is already truncated JSON.
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

/// Whether an approval is still waiting on a person.
///
/// Derived from the timeline rather than stored: a request with no matching
/// resolution on the same `call_id` is open. One source, so a card cannot
/// show a prompt that was already answered.
pub fn pending_approvals(events: &[baybo_store::project::IssueEventRow]) -> Vec<String> {
    let mut open: Vec<String> = Vec::new();
    for event in events {
        match &event.body {
            IssueEventBody::ApprovalRequested { call_id, .. } => {
                open.push(call_id.clone());
            }
            IssueEventBody::ApprovalResolved { call_id, .. } => {
                open.retain(|id| id != call_id);
            }
            _ => {}
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{IssueEventId, IssueId, ProjectId};
    use baybo_store::project::IssueEventRow;

    fn event(body: IssueEventBody) -> IssueEventRow {
        IssueEventRow {
            id: IssueEventId::generate(),
            issue_id: IssueId::generate(),
            project_id: ProjectId::parse("p").expect("id"),
            number: 1,
            actor: IssueActor::User,
            body,
            created_at: chrono::Utc::now(),
        }
    }

    fn requested(call_id: &str) -> IssueEventRow {
        event(IssueEventBody::ApprovalRequested {
            call_id: call_id.to_owned(),
            tool: "Bash".to_owned(),
            summary: "rm -rf build".to_owned(),
        })
    }

    fn resolved(call_id: &str, decision: ApprovalDecision) -> IssueEventRow {
        event(IssueEventBody::ApprovalResolved {
            call_id: call_id.to_owned(),
            decision,
        })
    }

    #[test]
    fn an_unanswered_request_is_the_only_pending_one() {
        let events = vec![
            requested("a"),
            resolved("a", ApprovalDecision::Approve),
            requested("b"),
        ];
        assert_eq!(pending_approvals(&events), vec!["b".to_owned()]);
    }

    #[test]
    fn a_denial_closes_a_prompt_as_surely_as_an_approval() {
        // Including the gate's deny-on-timeout, which is the case a
        // `pending` flag would most likely be left stale by.
        for decision in [
            ApprovalDecision::Approve,
            ApprovalDecision::ApproveAlways,
            ApprovalDecision::Deny,
        ] {
            let events = vec![requested("a"), resolved("a", decision)];
            assert!(pending_approvals(&events).is_empty(), "{decision:?}");
        }
    }

    #[test]
    fn two_prompts_at_once_resolve_independently() {
        // A turn can dispatch parallel tool calls, so the queue is not a
        // stack and a resolution must close its own call, not the newest.
        let events = vec![
            requested("a"),
            requested("b"),
            resolved("a", ApprovalDecision::Deny),
        ];
        assert_eq!(pending_approvals(&events), vec!["b".to_owned()]);
    }

    #[test]
    fn a_resolution_with_no_request_is_not_a_pending_approval() {
        // The timeline is append-only and readable from any offset, so a
        // page that starts mid-history can carry a resolution alone.
        assert!(pending_approvals(&[resolved("a", ApprovalDecision::Approve)]).is_empty());
        assert!(pending_approvals(&[]).is_empty());
    }

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

        // Blank or absent falls back to the preview rather than to nothing.
        for description in [None, Some("   ".to_owned())] {
            let req = ApprovalRequest {
                description,
                ..base.clone()
            };
            assert_eq!(summarise(&req), "{\"command\":\"rm -rf build\"}");
        }

        // …and a long one is cut with a mark, so a card never silently
        // shows half a sentence as if it were the whole thing.
        let long = ApprovalRequest {
            description: Some("x".repeat(MAX_SUMMARY_CHARS + 10)),
            ..base
        };
        let summary = summarise(&long);
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }
}
