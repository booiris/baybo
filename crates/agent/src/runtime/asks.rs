//! The agent's ports onto the human-ask ledger: the verbs a turn needs (not
//! the manager), the hydration probe, and the router door a dead ask's answer
//! comes back through. Implemented over [`HumanAskManager`].

use std::sync::Arc;

use async_trait::async_trait;
use baybo_channels::RouterInbound;
use baybo_human_ask::{HumanAskManager, ResumeSink, Settled};
use baybo_model::human_ask::{AskId, ResumeRequest};
use baybo_model::{SessionId, TriggerSource};
use baybo_store::HumanAskRow;
use tokio::sync::mpsc;
use tracing::warn;

use crate::SessionManager;

/// What a turn may do to its session's asks.
#[async_trait]
pub trait TurnAsks: Send + Sync {
    /// Claim `Abandoned` for the unapplied asks a cancelled call raised, and
    /// stamp them applied, right before the loop appends that call's result.
    /// The loop appends whatever this finds; a store failure is only logged,
    /// because a dangling `tool_use` would wedge the session's next LLM call.
    async fn abandon_for_tool_call(&self, session: &SessionId, tool_use_id: &str);

    /// The session's asks not yet applied to its transcript.
    async fn unapplied(&self, session: &SessionId) -> anyhow::Result<Vec<HumanAskRow>>;

    /// Abandon the session's dead pending asks and report what each
    /// unapplied ask needs appended — see [`HumanAskManager::settle_unapplied`].
    async fn settle_unapplied(&self, session: &SessionId) -> anyhow::Result<Vec<Settled>>;

    /// Stamp an ask applied; `false` when something else already did.
    async fn mark_applied(&self, ask_id: &AskId) -> anyhow::Result<bool>;

    async fn get(&self, ask_id: &AskId) -> anyhow::Result<Option<HumanAskRow>>;
}

#[async_trait]
impl TurnAsks for HumanAskManager {
    async fn abandon_for_tool_call(&self, session: &SessionId, tool_use_id: &str) {
        if let Err(e) = HumanAskManager::abandon_for_tool_call(self, session, tool_use_id).await {
            warn!(
                session_id = %session,
                tool_use_id,
                error = %e,
                "could not abandon the asks of a cancelled tool call; appending its result"
            );
        }
    }

    async fn unapplied(&self, session: &SessionId) -> anyhow::Result<Vec<HumanAskRow>> {
        Ok(self.unapplied_for_session(session).await?)
    }

    async fn settle_unapplied(&self, session: &SessionId) -> anyhow::Result<Vec<Settled>> {
        Ok(HumanAskManager::settle_unapplied(self, session).await?)
    }

    async fn mark_applied(&self, ask_id: &AskId) -> anyhow::Result<bool> {
        Ok(HumanAskManager::mark_applied(self, ask_id).await?)
    }

    async fn get(&self, ask_id: &AskId) -> anyhow::Result<Option<HumanAskRow>> {
        Ok(HumanAskManager::get(self, ask_id).await?)
    }
}

/// Hydration's [`baybo_context::PendingToolCalls`] probe, answered from the
/// ledger. Lives here so `baybo-human-ask` needs no edge to `baybo-context`.
pub struct UnappliedAsks(pub Arc<HumanAskManager>);

#[async_trait]
impl baybo_context::PendingToolCalls for UnappliedAsks {
    async fn unapplied_tool_use_ids(&self, session: &SessionId) -> Vec<String> {
        match self.0.unapplied_tool_use_ids(session).await {
            Ok(ids) => ids,
            Err(e) => {
                warn!(
                    session_id = %session,
                    error = %e,
                    "could not read the session's unapplied asks; hydration fills every dangling call"
                );
                Vec::new()
            }
        }
    }
}

/// The chat Resume door: an answered dead ask becomes a
/// [`RouterInbound::ResumeAsk`] on the router's intake, which spawns or
/// hydrates the session's actor.
pub struct RouterResumeSink {
    inbound: mpsc::Sender<RouterInbound>,
    sessions: Arc<SessionManager>,
}

impl RouterResumeSink {
    pub fn new(inbound: mpsc::Sender<RouterInbound>, sessions: Arc<SessionManager>) -> Self {
        Self { inbound, sessions }
    }
}

#[async_trait]
impl ResumeSink for RouterResumeSink {
    async fn resumes_on_answer(&self, session_id: &SessionId) -> bool {
        match self.sessions.get(session_id).await {
            Ok(Some(session)) => !matches!(session.trigger, TriggerSource::Issue { .. }),
            Ok(None) => false,
            Err(e) => {
                warn!(
                    %session_id,
                    error = %e,
                    "could not read the session to route an answer; its next turn applies it"
                );
                false
            }
        }
    }

    fn request_resume(&self, req: ResumeRequest) {
        if let Err(e) = self.inbound.try_send(RouterInbound::ResumeAsk(req)) {
            warn!(
                error = %e,
                "could not queue a resume; the session's next turn or the next boot applies the answer"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ChannelType, User};
    use baybo_session::test_support::{MemorySessionFolderStore, MemorySessionStore};

    fn sessions() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        ))
    }

    fn user() -> User {
        User {
            id: "u".into(),
            name: None,
            channel: ChannelType::tui(),
        }
    }

    #[tokio::test]
    async fn a_chat_session_resumes_and_an_unknown_one_does_not() {
        let sessions = sessions();
        let session = sessions
            .create_session(user(), ChannelType::tui())
            .await
            .expect("create");
        let (tx, _rx) = mpsc::channel(4);
        let sink = RouterResumeSink::new(tx, Arc::clone(&sessions));
        assert!(sink.resumes_on_answer(&session.id).await);
        assert!(!sink.resumes_on_answer(&SessionId::from("missing")).await);
    }

    #[tokio::test]
    async fn an_issue_session_resumes_through_its_run_instead() {
        let sessions = sessions();
        let session = sessions
            .create_session_with_trigger(
                user(),
                ChannelType::tui(),
                TriggerSource::Issue {
                    project_id: baybo_model::ProjectId::generate(),
                    issue_id: baybo_model::IssueId::generate(),
                    number: 1,
                },
            )
            .await
            .expect("create");
        let (tx, _rx) = mpsc::channel(4);
        let sink = RouterResumeSink::new(tx, Arc::clone(&sessions));
        assert!(!sink.resumes_on_answer(&session.id).await);
    }

    #[tokio::test]
    async fn a_resume_request_reaches_the_router_intake() {
        let (tx, mut rx) = mpsc::channel(4);
        let sink = RouterResumeSink::new(tx, sessions());
        let req = ResumeRequest {
            session_id: SessionId::from("s"),
            ask_id: AskId::new(),
        };
        sink.request_resume(req.clone());
        match rx.try_recv() {
            Ok(RouterInbound::ResumeAsk(got)) => assert_eq!(got, req),
            other => panic!("expected a ResumeAsk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_full_intake_drops_the_request_without_blocking() {
        let (tx, _rx) = mpsc::channel(1);
        let sink = RouterResumeSink::new(tx, sessions());
        for _ in 0..3 {
            sink.request_resume(ResumeRequest {
                session_id: SessionId::from("s"),
                ask_id: AskId::new(),
            });
        }
    }
}
