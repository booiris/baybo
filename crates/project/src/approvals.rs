//! Putting a run's approval prompts on the card that asked for them.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use baybo_model::{ApprovalDecision, ApprovalResolution, ProjectId, SessionId};
use baybo_store::SessionStore;
use baybo_store::project::{IssueActor, IssueEventBody};
use baybo_tools::{ApprovalGate, ApprovalOutcome, ApprovalRequest};
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::ProjectManager;

const MAX_SUMMARY_CHARS: usize = 160;

struct OpenPrompt {
    call_id: String,
    project: ProjectId,
    number: i64,
    actor: IssueActor,
    /// Dropping this cancels the wrapped gate's request.
    cancel: oneshot::Sender<()>,
}

/// Closes a card's parked approval prompts and cancels their requests.
#[async_trait]
pub trait CardPromptCloser: Send + Sync {
    async fn close_card_prompts(&self, project: &ProjectId, number: i64);
}

/// Wraps a channel's approval gate and writes what it sees onto the issue's
/// timeline.
pub struct TimelineApprovalGate {
    /// The gate that actually prompts. Held directly rather than resolved
    /// per call, because resolving would find *this* wrapper.
    inner: Arc<dyn ApprovalGate>,
    manager: Arc<ProjectManager>,
    sessions: Arc<dyn SessionStore>,
    /// Removing an entry claims its single resolution across all close paths.
    open: Arc<Mutex<Vec<OpenPrompt>>>,
}

fn claim(open: &Mutex<Vec<OpenPrompt>>, call_id: &str) -> Option<OpenPrompt> {
    let mut open = open.lock();
    let pos = open.iter().position(|p| p.call_id == call_id)?;
    Some(open.remove(pos))
}

async fn record_resolved(
    manager: &ProjectManager,
    prompt: OpenPrompt,
    decision: ApprovalDecision,
    resolution: ApprovalResolution,
) {
    let OpenPrompt {
        call_id,
        project,
        number,
        actor,
        cancel,
    } = prompt;
    // Cancel the parked tool before recording its resolution.
    drop(cancel);
    manager
        .record_event(
            &project,
            number,
            actor,
            IssueEventBody::ApprovalResolved {
                call_id,
                decision,
                resolution,
            },
        )
        .await;
}

async fn close_card(
    manager: &ProjectManager,
    open: &Mutex<Vec<OpenPrompt>>,
    project: &ProjectId,
    number: i64,
) {
    let mine = {
        let mut open = open.lock();
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *open)
            .into_iter()
            .partition(|p| &p.project == project && p.number == number);
        *open = rest;
        mine
    };
    for prompt in mine {
        record_resolved(
            manager,
            prompt,
            ApprovalDecision::Deny,
            ApprovalResolution::Abandoned,
        )
        .await;
    }
}

/// Manager-owned prompt closer; the weak manager reference avoids a cycle.
struct ParkedPrompts {
    open: Arc<Mutex<Vec<OpenPrompt>>>,
    manager: Weak<ProjectManager>,
}

#[async_trait]
impl CardPromptCloser for ParkedPrompts {
    async fn close_card_prompts(&self, project: &ProjectId, number: i64) {
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        close_card(&manager, &self.open, project, number).await;
    }
}

impl TimelineApprovalGate {
    /// Wrap `inner` and register its prompt closer with the board.
    pub fn new(
        inner: Arc<dyn ApprovalGate>,
        manager: Arc<ProjectManager>,
        sessions: Arc<dyn SessionStore>,
    ) -> Self {
        let open = Arc::new(Mutex::new(Vec::new()));
        manager.set_prompt_closer(Arc::new(ParkedPrompts {
            open: Arc::clone(&open),
            manager: Arc::downgrade(&manager),
        }));
        Self {
            inner,
            manager,
            sessions,
            open,
        }
    }

    /// Await closure of prompts that `Drop` cannot safely persist at shutdown.
    pub async fn close_open_prompts(&self) {
        let parked = std::mem::take(&mut *self.open.lock());
        for prompt in parked {
            record_resolved(
                &self.manager,
                prompt,
                ApprovalDecision::Deny,
                ApprovalResolution::Abandoned,
            )
            .await;
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
        // Register before announcing so cancellation cannot orphan the prompt.
        let (cancel, cancelled) = oneshot::channel();
        self.open.lock().push(OpenPrompt {
            call_id: call_id.clone(),
            project: project.clone(),
            number,
            actor: actor.clone(),
            cancel,
        });
        self.manager
            .record_event(
                &project,
                number,
                actor,
                IssueEventBody::ApprovalRequested {
                    call_id: call_id.clone(),
                    attempt,
                    tool: req.tool.clone(),
                    summary: summarise(&req),
                },
            )
            .await;

        // Live cancellation uses `Drop`; shutdown uses the awaited close path.
        let _guard = AbandonGuard {
            manager: Arc::clone(&self.manager),
            open: Arc::clone(&self.open),
            call_id: call_id.clone(),
        };

        let outcome = tokio::select! {
            biased;
            _ = cancelled => ApprovalOutcome::abandoned(),
            outcome = self.inner.request(req) => outcome,
        };

        // A closer that won the claim also overrides any raced approval.
        match claim(&self.open, &call_id) {
            Some(prompt) => {
                record_resolved(&self.manager, prompt, outcome.decision, outcome.resolution).await;
                outcome
            }
            None => ApprovalOutcome::abandoned(),
        }
    }
}

#[async_trait]
impl CardPromptCloser for TimelineApprovalGate {
    async fn close_card_prompts(&self, project: &ProjectId, number: i64) {
        close_card(&self.manager, &self.open, project, number).await;
    }
}

/// Closes a prompt dropped during live cancellation.
struct AbandonGuard {
    manager: Arc<ProjectManager>,
    open: Arc<Mutex<Vec<OpenPrompt>>>,
    call_id: String,
}

impl Drop for AbandonGuard {
    fn drop(&mut self) {
        let Some(prompt) = claim(&self.open, &self.call_id) else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let manager = Arc::clone(&self.manager);
        handle.spawn(async move {
            record_resolved(
                &manager,
                prompt,
                ApprovalDecision::Deny,
                ApprovalResolution::Abandoned,
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
