//! Runtime side of background jobs: the [`BackgroundJobSink`] every
//! background-capable tool hands its work to once it runs past its turn, and
//! the escort that sees each job to its end and routes an
//! [`AgentMessage::BackgroundJobFinished`] back to the parent session.
//! Whether a turn may create background work at all is
//! [`background_eligible`]; the foreground window before a job gets here is
//! [`baybo_tools::background::run`].
//!
//! The escort uses the supervisor's in-flight registry for two things:
//! pinning the parent against the idle reaper while the job runs, and `/stop`
//! suppression — `/stop` / `JobStop` cancel the registered token and drain
//! the entry, so the escort sees the job end and skips delivery (a
//! user-stopped result must not surface). Process shutdown cancels the job
//! too, and is never reported as a failure.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use baybo_model::Session;
use baybo_session::SessionManager;
use baybo_tools::background::{BackgroundJobSink, DetachedJob};
use baybo_tools::{BackgroundJobControl, BackgroundJobInfo};
use baybo_turn::TurnInputKind;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::actor::AgentMessage;
use crate::actor::supervisor::{AgentSupervisor, InFlightJob};

/// Whether this turn may **create** background work — a `Bash` command that
/// converts to background on timeout, or a subagent that converts (or is
/// dispatched) instead of blocking. Computed once per turn by the agent loop
/// and carried to both consumers as [`baybo_tools::ToolContext::background_eligible`].
/// Both halves of the gate:
///
/// * the **session** must be somewhere a completion notification can land
///   ([`Session::can_host_background_jobs`]);
/// * the **turn** must not be a cron fire's own turn. Converting there would
///   end the fire with a partial report and deliver the real answer as a
///   separate notification, so a fire blocks until its work is done.
///
/// A user reply inside a recurring fire's conversation is an ordinary
/// `UserChat` turn in a session that *can* host the notification, so it
/// backgrounds like any other chat.
pub(crate) fn background_eligible(session: &Session, turn_kind: TurnInputKind) -> bool {
    session.can_host_background_jobs() && !matches!(turn_kind, TurnInputKind::Cron)
}

/// Implements [`BackgroundJobSink`] and [`BackgroundJobControl`]. Constructed
/// before the supervisor exists in the boot sequence, so it holds the
/// supervisor behind a `OnceLock` set right after the supervisor is built.
pub struct BackgroundJobManager {
    supervisor: Arc<OnceLock<AgentSupervisor>>,
    session_manager: Arc<SessionManager>,
    shutdown: CancellationToken,
}

impl BackgroundJobManager {
    pub fn new(
        supervisor: Arc<OnceLock<AgentSupervisor>>,
        session_manager: Arc<SessionManager>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            supervisor,
            session_manager,
            shutdown,
        }
    }
}

#[async_trait]
impl BackgroundJobSink for BackgroundJobManager {
    async fn adopt(&self, job: DetachedJob) {
        let Some(sup) = self.supervisor.get().cloned() else {
            warn!(
                handle = %job.handle,
                "background job dispatched before supervisor wired; result will be lost"
            );
            return;
        };
        sup.note_background_job_started(
            &job.parent,
            InFlightJob {
                handle: job.handle.clone(),
                kind: job.kind.clone(),
                task_summary: job.summary.clone(),
                session: job.session.clone(),
                cancel_token: job.cancel.clone(),
            },
        );
        // Registering and spawning the escort happen with no await between
        // them: `adopt` runs inside a tool call the agent loop may drop at any
        // await, and a registered job with no escort would never clear.
        let session_manager = Arc::clone(&self.session_manager);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            // A job going to the background is activity on the parent: it
            // keeps the session's recency honest while nothing else is said.
            if let Err(e) = session_manager.touch(&job.parent).await {
                warn!(
                    parent_session_id = %job.parent,
                    error = %e,
                    "background job: failed to touch parent session"
                );
            }
            escort(sup, shutdown, job).await;
        });
    }
}

#[async_trait]
impl BackgroundJobControl for BackgroundJobManager {
    async fn list(&self, session_id: &baybo_model::SessionId) -> Vec<BackgroundJobInfo> {
        let Some(sup) = self.supervisor.get() else {
            return Vec::new();
        };
        sup.list_in_flight_background(session_id)
            .into_iter()
            .map(|info| BackgroundJobInfo {
                handle: info.handle,
                kind: info.kind,
                summary: info.task_summary,
            })
            .collect()
    }

    async fn stop(&self, session_id: &baybo_model::SessionId, handle: &str) -> bool {
        let Some(sup) = self.supervisor.get() else {
            return false;
        };
        sup.cancel_in_flight_background(session_id, handle)
            .is_some()
    }
}

/// See the job to its end, then deliver its result to the parent — unless
/// `/stop` / `JobStop` drained its in-flight entry, or the process is
/// shutting down. The entry is cleared only AFTER delivery, so the reaper
/// cannot tear the parent down between the job ending and its result
/// reaching the mailbox.
async fn escort(supervisor: AgentSupervisor, shutdown: CancellationToken, job: DetachedJob) {
    let DetachedJob {
        handle,
        parent,
        cohort,
        cancel,
        result,
        ..
    } = job;
    // A job that panics still has to leave the registry, or its parent stays
    // pinned against the reaper and listed in `JobList` forever.
    let mut result = std::panic::AssertUnwindSafe(result).catch_unwind();
    let outcome = tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            cancel.cancel();
            let _ = result.await;
            None
        }
        outcome = &mut result => Some(outcome),
    };
    let Some(outcome) = outcome.filter(|_| !shutdown.is_cancelled()) else {
        supervisor.note_background_job_finished(&parent, &handle);
        debug!(
            parent_session_id = %parent,
            %handle,
            "background job stopped during process shutdown; suppressing delivery"
        );
        return;
    };
    let Ok(mut pending) = outcome else {
        supervisor.note_background_job_finished(&parent, &handle);
        warn!(
            parent_session_id = %parent,
            %handle,
            "background job panicked; nothing to deliver"
        );
        return;
    };
    if !supervisor.is_background_job_in_flight(&parent, &handle) {
        debug!(
            parent_session_id = %parent,
            %handle,
            "background job was stopped; suppressing delivery"
        );
        return;
    }
    pending.group = cohort;
    let delivered = supervisor
        .route(
            &parent,
            AgentMessage::BackgroundJobFinished(Box::new(pending)),
        )
        .await;
    if !delivered {
        warn!(
            parent_session_id = %parent,
            %handle,
            "background job result could not be routed — parent actor not registered"
        );
    }
    supervisor.note_background_job_finished(&parent, &handle);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use baybo_model::{PendingBackgroundResult, SessionId, SubagentExitStatus};
    use baybo_session::test_support::{MemorySessionFolderStore, MemorySessionStore};
    use futures::FutureExt;
    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::actor::mailbox::{self, MailboxReceiver};

    struct Fixture {
        manager: BackgroundJobManager,
        supervisor: AgentSupervisor,
        parent: SessionId,
        mailbox: MailboxReceiver<AgentMessage>,
    }

    fn fixture() -> Fixture {
        let (tx, _rx) = mpsc::channel(8);
        let supervisor = AgentSupervisor::new(tx);
        let parent = SessionId::from("parent");
        let (sender, mailbox) = mailbox::channel(8);
        supervisor
            .register_if_absent(parent.clone(), sender)
            .expect("parent registers once");
        let slot = Arc::new(OnceLock::new());
        assert!(slot.set(supervisor.clone()).is_ok());
        let sessions = Arc::new(SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        ));
        Fixture {
            manager: BackgroundJobManager::new(slot, sessions, CancellationToken::new()),
            supervisor,
            parent,
            mailbox,
        }
    }

    /// A job whose result is ready once `finish` fires — or, being cancelled,
    /// once its token does.
    fn job(
        parent: &SessionId,
        cohort: Option<baybo_model::CohortKey>,
    ) -> (DetachedJob, oneshot::Sender<()>) {
        let (finish, finished) = oneshot::channel::<()>();
        let cancel = CancellationToken::new();
        let result = {
            let cancel = cancel.clone();
            async move {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = finished => {}
                }
                PendingBackgroundResult::command(
                    "bg-1",
                    "make",
                    0,
                    "/dev/null",
                    "ok",
                    SubagentExitStatus::Completed,
                )
            }
            .boxed()
        };
        let job = DetachedJob {
            handle: "bg-1".into(),
            parent: parent.clone(),
            kind: "command".into(),
            summary: "make".into(),
            session: None,
            cohort,
            cancel,
            result,
        };
        (job, finish)
    }

    async fn settle(supervisor: &AgentSupervisor, parent: &SessionId) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while supervisor.has_in_flight_background_jobs(parent) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the escort clears the job");
    }

    #[tokio::test]
    async fn a_finished_job_is_delivered_to_its_parent_with_its_cohort() {
        let mut fx = fixture();
        let cohort = baybo_model::CohortKey::new(
            baybo_model::TurnId::new(),
            &baybo_model::BackgroundGroupName::parse("g").unwrap(),
        );
        let (job, finish) = job(&fx.parent, Some(cohort.clone()));
        fx.manager.adopt(job).await;
        assert!(
            fx.supervisor
                .is_background_job_in_flight(&fx.parent, &"bg-1".into())
        );

        finish.send(()).unwrap();
        settle(&fx.supervisor, &fx.parent).await;
        let Ok(AgentMessage::BackgroundJobFinished(pending)) = fx.mailbox.try_recv() else {
            panic!("the result reaches the parent's mailbox");
        };
        assert_eq!(pending.handle_id, "bg-1");
        assert_eq!(pending.group, Some(cohort));
    }

    /// A job whose work panics still leaves the registry, so its parent is
    /// neither pinned against the reaper nor listed in `JobList` forever.
    #[tokio::test]
    async fn a_panicking_job_still_clears_its_registry_entry() {
        let mut fx = fixture();
        let (mut job, _finish) = job(&fx.parent, None);
        job.result = async { panic!("the job blew up") }.boxed();
        fx.manager.adopt(job).await;

        settle(&fx.supervisor, &fx.parent).await;
        assert!(fx.mailbox.try_recv().is_err(), "nothing to deliver");
    }

    #[tokio::test]
    async fn a_stopped_job_ends_and_delivers_nothing() {
        let mut fx = fixture();
        let (job, _finish) = job(&fx.parent, None);
        fx.manager.adopt(job).await;

        assert!(
            fx.manager.stop(&fx.parent, "bg-1").await,
            "JobStop finds it"
        );
        // The stop drained the entry at once; give the escort time to see the
        // cancelled job end and decide what to do with its result.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            fx.mailbox.try_recv().is_err(),
            "a user-stopped result must not surface"
        );
    }
}

#[cfg(test)]
mod background_eligible_tests {
    //! The turn half of the background-job gate. The session half lives on
    //! `Session::can_host_background_jobs` and is covered next to the
    //! spawner; what's pinned here is that the two compose so a recurring
    //! cron job's *fire* blocks while a *reply* in the same conversation
    //! backgrounds.
    use super::background_eligible;
    use baybo_model::{
        ChannelType, Lineage, LineageKind, Session, SessionId, SessionState, TriggerSource, TurnId,
        User,
    };
    use baybo_turn::TurnInputKind;
    use chrono::Utc;

    fn session_with(trigger: TriggerSource, lineage: Option<Lineage>) -> Session {
        let now = Utc::now();
        let id = SessionId::from("sess-bg");
        Session {
            id: id.clone(),
            user: User {
                id: "user-bg".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: id,
            trigger,
            lineage,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    fn cron(conversation: bool) -> Session {
        session_with(
            TriggerSource::Cron {
                cron_job_id: "c-1".into(),
                origin_session_id: None,
                conversation,
                job_title: None,
                project_id: None,
            },
            None,
        )
    }

    #[test]
    fn a_user_chat_backgrounds() {
        let s = session_with(TriggerSource::User, None);
        assert!(background_eligible(&s, TurnInputKind::UserChat));
    }

    /// The point of the turn half: a fire that backgrounded its slow work
    /// would notify with a partial report and deliver the real answer
    /// separately, so it blocks even though its conversation could host the
    /// notification.
    #[test]
    fn a_recurring_fires_own_turn_blocks() {
        assert!(!background_eligible(&cron(true), TurnInputKind::Cron));
    }

    /// …and the same conversation backgrounds once the user takes over. This
    /// pair is the whole feature.
    #[test]
    fn a_reply_inside_a_recurring_fires_conversation_backgrounds() {
        assert!(background_eligible(&cron(true), TurnInputKind::UserChat));
    }

    /// A one-shot fire's workspace is invisible and unregistered, so nothing
    /// can be delivered there — no turn kind unlocks it.
    #[test]
    fn a_one_shot_fires_workspace_never_backgrounds() {
        assert!(!background_eligible(&cron(false), TurnInputKind::Cron));
        assert!(!background_eligible(&cron(false), TurnInputKind::UserChat));
    }

    /// A subagent's turn ends with the child, so there is no later turn to
    /// notify into regardless of what started it.
    #[test]
    fn a_subagent_session_never_backgrounds() {
        let s = session_with(
            TriggerSource::User,
            Some(Lineage {
                parent_session_id: SessionId::from("parent"),
                parent_turn_id: TurnId::new(),
                parent_span_id: None,
                kind: LineageKind::Subagent,
            }),
        );
        assert!(!background_eligible(&s, TurnInputKind::UserChat));
    }

    /// A background result's own notification turn must stay eligible: the
    /// agent reacting to one job may dispatch the next.
    #[test]
    fn a_notification_turn_still_backgrounds() {
        let s = session_with(TriggerSource::User, None);
        assert!(background_eligible(&s, TurnInputKind::SubagentNotification));
    }
}
