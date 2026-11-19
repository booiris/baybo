//! The agent's side of the chat Resume door: the sink the ask manager sends
//! an answered dead ask through, and the router's rule for what it opens.

use baybo_human_ask::ResumeSink;
use baybo_model::Session;
use baybo_model::human_ask::ResumeRequest;
use tokio::sync::mpsc;
use tracing::warn;

use crate::actor::AgentMessage;

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
    use baybo_model::{ChannelType, SessionId, TriggerSource, User};
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
