//! The agent's adapters onto the human-ask ledger: the hydration probe and
//! the router door a dead ask's answer comes back through. Both are
//! implemented over [`HumanAskManager`].

use std::sync::Arc;

use async_trait::async_trait;
use baybo_human_ask::{HumanAskManager, ResumeSink};
use baybo_model::human_ask::ResumeRequest;
use baybo_model::{Session, SessionId};
use tokio::sync::mpsc;
use tracing::warn;

use crate::actor::AgentMessage;

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

/// How many answered dead asks may wait for the router at once. A request
/// past it is dropped, which loses nothing: the row stays resolved, and the
/// session's next turn or the next boot applies it.
const ASK_RESUME_BUFFER: usize = 256;

/// The chat Resume door: an answered dead ask is queued for the router,
/// which spawns or hydrates the session's actor.
pub struct RouterResumeSink(mpsc::Sender<ResumeRequest>);

impl RouterResumeSink {
    /// The sink the ask manager holds and the receiver the router drains.
    pub fn channel() -> (Self, mpsc::Receiver<ResumeRequest>) {
        let (tx, rx) = mpsc::channel(ASK_RESUME_BUFFER);
        (Self(tx), rx)
    }
}

impl ResumeSink for RouterResumeSink {
    fn request_resume(&self, req: ResumeRequest) {
        if let Err(e) = self.0.try_send(req) {
            warn!(
                error = %e,
                "could not queue a resume; the session's next turn or the next boot applies the answer"
            );
        }
    }
}

/// The message that resumes `session` on the answer in `req`, or `None` when
/// the answer is applied some other way: an issue session's own run re-drive
/// applies it under the real run, so it opens no chat Resume turn.
pub fn resume_message(session: &Session, req: &ResumeRequest) -> Option<AgentMessage> {
    if session.trigger.is_issue_session() {
        return None;
    }
    Some(AgentMessage::ResumeAsk {
        ask_id: req.ask_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::human_ask::AskId;
    use baybo_model::{ChannelType, TriggerSource, User};
    use baybo_session::SessionManager;
    use baybo_session::test_support::{MemorySessionFolderStore, MemorySessionStore};
    use std::sync::Arc;

    async fn session(trigger: TriggerSource) -> Session {
        let sessions = SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        );
        let user = User {
            id: "u".into(),
            name: None,
            channel: ChannelType::tui(),
        };
        sessions
            .create_session_with_trigger(user, ChannelType::tui(), trigger)
            .await
            .expect("create")
    }

    fn request() -> ResumeRequest {
        ResumeRequest {
            session_id: SessionId::from("s"),
            ask_id: AskId::new(),
        }
    }

    #[tokio::test]
    async fn a_chat_session_resumes_on_its_answer() {
        let req = request();
        let message = resume_message(&session(TriggerSource::User).await, &req);
        assert!(
            matches!(message, Some(AgentMessage::ResumeAsk { ask_id }) if ask_id == req.ask_id)
        );
    }

    #[tokio::test]
    async fn an_issue_session_resumes_through_its_run_instead() {
        let trigger = TriggerSource::Issue {
            project_id: baybo_model::ProjectId::generate(),
            issue_id: baybo_model::IssueId::generate(),
            number: 1,
        };
        assert!(resume_message(&session(trigger).await, &request()).is_none());
    }

    #[tokio::test]
    async fn a_resume_request_reaches_the_router() {
        let (sink, mut rx) = RouterResumeSink::channel();
        let req = request();
        sink.request_resume(req.clone());
        assert_eq!(rx.try_recv().ok(), Some(req));
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_request_without_blocking() {
        let (sink, _rx) = RouterResumeSink::channel();
        for _ in 0..=ASK_RESUME_BUFFER {
            sink.request_resume(request());
        }
    }
}
