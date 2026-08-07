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
