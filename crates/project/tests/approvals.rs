//! A parked approval prompt is closed by exactly one closer — never by none.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{
    ApprovalDecision, ApprovalResolution, ChannelType, ProjectId, SessionId, TriggerSource, User,
};
use baybo_project::{
    CardPromptCloser, NewIssueRequest, NewProject, ProjectManager, TimelineApprovalGate,
};
use baybo_store::SessionStore;
use baybo_store::project::{
    IssueActor, IssueEventBody, IssuePriority, IssueRunRow, IssueStatus, RunStatus,
};
use baybo_tools::{ApprovalGate, ApprovalOutcome, ApprovalRequest};
use baybo_workspace::WorkspacePaths;
use tokio::sync::oneshot;

/// A gate that never receives an answer.
struct ParkedGate;

#[async_trait]
impl ApprovalGate for ParkedGate {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _ = rx.await;
        unreachable!("the parked gate is never released")
    }
}

struct FixedGate(ApprovalOutcome);

#[async_trait]
impl ApprovalGate for FixedGate {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
        self.0
    }
}

struct Fixture {
    manager: Arc<ProjectManager>,
    sessions: Arc<dyn SessionStore>,
    project: ProjectId,
    _workspace: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let workspace = tempfile::tempdir().expect("tempdir");
    let paths = WorkspacePaths::new(workspace.path().to_path_buf());
    tokio::fs::create_dir_all(paths.work_dir())
        .await
        .expect("work dir");
    let store = baybo_storage::Store::open(workspace.path().join("storage.db"))
        .await
        .expect("store");
    let manager = Arc::new(ProjectManager::new(
        Arc::clone(&store.project),
        Arc::clone(&store.agent_profile),
        Arc::clone(&store.blob),
        paths,
        Arc::new(baybo_project::NoopProjectEvents),
        baybo_project::no_dispatch(),
    ));
    let project = manager
        .create_project(NewProject {
            name: "Approvals".to_owned(),
            description: String::new(),
            workdir: None,
            daily_budget: None,
            daily_budget_tokens: None,
            max_parallel_issue_runs: Some(0),
        })
        .await
        .expect("create project")
        .id;
    Fixture {
        manager,
        sessions: Arc::new(baybo_session::test_support::MemorySessionStore::new()),
        project,
        _workspace: workspace,
    }
}

impl Fixture {
    async fn card(&self, title: &str) -> (i64, SessionId) {
        self.open_card(title, IssueStatus::Backlog, None).await
    }

    async fn card_with_a_run(&self, title: &str) -> (i64, SessionId, IssueRunRow) {
        let lead = self
            .manager
            .team(&self.project)
            .await
            .expect("team")
            .into_iter()
            .find(|row| {
                row.team
                    .as_ref()
                    .is_some_and(|t| t.handle.as_str() == baybo_project::LEAD_HANDLE)
            })
            .expect("every board opens with a lead")
            .id;
        let (number, session) = self
            .open_card(title, IssueStatus::InProgress, Some(lead))
            .await;
        let run = self
            .manager
            .list_runs(&self.project, number)
            .await
            .expect("runs")
            .into_iter()
            .next()
            .expect("entering In Progress records a run");
        (number, session, run)
    }

    async fn open_card(
        &self,
        title: &str,
        status: IssueStatus,
        assignee: Option<baybo_model::AgentProfileId>,
    ) -> (i64, SessionId) {
        let issue = self
            .manager
            .create_issue(
                &self.project,
                IssueActor::User,
                NewIssueRequest {
                    title: title.to_owned(),
                    description: String::new(),
                    attachments: Vec::new(),
                    status,
                    priority: IssuePriority::None,
                    assignee,
                    parent: None,
                    stage: 0,
                    source_key: None,
                },
            )
            .await
            .expect("create issue")
            .issue()
            .clone();
        let session_id = SessionId::from(format!("sess-{}", issue.number));
        let now = chrono::Utc::now();
        self.sessions
            .save(&baybo_model::Session {
                id: session_id.clone(),
                user: User {
                    id: "u".to_owned(),
                    name: None,
                    channel: ChannelType::owner(),
                },
                channel: ChannelType::owner(),
                created_at: now,
                last_active: now,
                state: Default::default(),
                root_session_id: session_id.clone(),
                trigger: TriggerSource::Issue {
                    project_id: self.project.clone(),
                    issue_id: issue.id.clone(),
                    number: issue.number,
                },
                lineage: None,
                hidden: false,
                pinned: false,
                archived: false,
                folder_id: None,
                title: None,
            })
            .await
            .expect("session");
        (issue.number, session_id)
    }

    fn gate(&self, inner: Arc<dyn ApprovalGate>) -> Arc<TimelineApprovalGate> {
        Arc::new(TimelineApprovalGate::new(
            inner,
            Arc::clone(&self.manager),
            Arc::clone(&self.sessions),
        ))
    }

    async fn resolutions(
        &self,
        number: i64,
    ) -> Vec<(String, ApprovalDecision, ApprovalResolution)> {
        self.manager
            .timeline(&self.project, number)
            .await
            .expect("timeline")
            .into_iter()
            .filter_map(|row| match row.body {
                IssueEventBody::ApprovalResolved {
                    call_id,
                    decision,
                    resolution,
                } => Some((call_id, decision, resolution)),
                _ => None,
            })
            .collect()
    }

    async fn requested_call_ids(&self, number: i64) -> Vec<String> {
        self.manager
            .timeline(&self.project, number)
            .await
            .expect("timeline")
            .into_iter()
            .filter_map(|row| match row.body {
                IssueEventBody::ApprovalRequested { call_id, .. } => Some(call_id),
                _ => None,
            })
            .collect()
    }
}

fn request(session_id: &SessionId, call_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        call_id: call_id.to_owned(),
        tool_call_id: None,
        session_id: session_id.clone(),
        user_id: "u".to_owned(),
        tool: "Bash".to_owned(),
        accesses: Vec::new(),
        params_preview: "{\"command\":\"rm -rf build\"}".to_owned(),
        description: None,
    }
}

/// Wait for prompt announcement with a test deadline.
async fn wait_for_announcement(f: &Fixture, number: i64) -> String {
    for _ in 0..200 {
        if let Some(call_id) = f.requested_call_ids(number).await.into_iter().next() {
            return call_id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the prompt was never announced on the card");
}

#[tokio::test]
async fn a_shutdown_closes_a_prompt_that_is_still_parked() {
    let f = fixture().await;
    let (number, session_id) = f.card("Parked").await;
    let gate = f.gate(Arc::new(ParkedGate));

    let parked = tokio::spawn({
        let gate = Arc::clone(&gate);
        let session_id = session_id.clone();
        async move { gate.request(request(&session_id, "call-1")).await }
    });
    let call_id = wait_for_announcement(&f, number).await;

    gate.close_open_prompts().await;

    assert_eq!(
        f.resolutions(number).await,
        vec![(
            call_id,
            ApprovalDecision::Deny,
            ApprovalResolution::Abandoned
        )],
        "shutdown must not leave the request dangling"
    );
    parked.abort();
    let _ = parked.await;
}

#[tokio::test]
async fn a_prompt_is_resolved_exactly_once() {
    let f = fixture().await;
    let (number, session_id) = f.card("Parked").await;
    let gate = f.gate(Arc::new(ParkedGate));

    let parked = tokio::spawn({
        let gate = Arc::clone(&gate);
        let session_id = session_id.clone();
        async move { gate.request(request(&session_id, "call-1")).await }
    });
    wait_for_announcement(&f, number).await;
    gate.close_open_prompts().await;

    parked.abort();
    let _ = parked.await;
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    assert_eq!(
        f.resolutions(number).await.len(),
        1,
        "the claim is what makes the second closer a no-op"
    );
}

#[tokio::test]
async fn an_answered_prompt_records_the_real_outcome() {
    let f = fixture().await;
    let (number, session_id) = f.card("Answered").await;
    let gate = f.gate(Arc::new(FixedGate(ApprovalOutcome::answered(
        ApprovalDecision::Approve,
    ))));

    let outcome = gate.request(request(&session_id, "call-1")).await;
    assert_eq!(
        outcome,
        ApprovalOutcome::answered(ApprovalDecision::Approve)
    );

    let resolved = f.resolutions(number).await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].1, ApprovalDecision::Approve);
    assert_eq!(resolved[0].2, ApprovalResolution::Answered);

    gate.close_open_prompts().await;
    assert_eq!(f.resolutions(number).await.len(), 1);
}

#[tokio::test]
async fn a_settling_run_takes_its_own_parked_prompt_with_it() {
    let f = fixture().await;
    let (number, session_id, run) = f.card_with_a_run("Settles mid-prompt").await;
    let _gate = f.gate(Arc::new(ParkedGate));

    let parked = tokio::spawn({
        let gate = Arc::clone(&_gate);
        let session_id = session_id.clone();
        async move { gate.request(request(&session_id, "call-1")).await }
    });
    let call_id = wait_for_announcement(&f, number).await;

    f.manager
        .finish_run(
            &run,
            std::path::Path::new("/nonexistent/checkout"),
            chrono::Utc::now(),
            baybo_project::RunOutcome {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
        )
        .await;

    assert_eq!(
        f.resolutions(number).await,
        vec![(
            call_id,
            ApprovalDecision::Deny,
            ApprovalResolution::Abandoned
        )],
        "the run that would have answered it is gone, so the prompt goes with it"
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), parked)
            .await
            .expect("the request is un-parked, not merely written off")
            .expect("the request task"),
        ApprovalOutcome::abandoned(),
        "and the tool waiting on it is told, rather than parked against a card that says Deny"
    );
}

/// A gate that approves when released by the test.
struct ApprovesWhenReleased(parking_lot::Mutex<Option<oneshot::Receiver<()>>>);

#[async_trait]
impl ApprovalGate for ApprovesWhenReleased {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
        let released = self.0.lock().take().expect("one request");
        let _ = released.await;
        ApprovalOutcome::answered(ApprovalDecision::Approve)
    }
}

#[tokio::test]
async fn an_approval_that_races_the_closer_never_reaches_the_tool() {
    let f = fixture().await;
    let (number, session_id) = f.card("Raced").await;
    let (release, released) = oneshot::channel();
    let gate = f.gate(Arc::new(ApprovesWhenReleased(parking_lot::Mutex::new(
        Some(released),
    ))));

    let asked = tokio::spawn({
        let gate = Arc::clone(&gate);
        let session_id = session_id.clone();
        async move { gate.request(request(&session_id, "call-1")).await }
    });
    wait_for_announcement(&f, number).await;

    gate.close_card_prompts(&f.project, number).await;
    let _ = release.send(());

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), asked)
            .await
            .expect("the request ends with the claim, not with the human")
            .expect("the request task"),
        ApprovalOutcome::abandoned(),
        "the ledger already says this prompt was refused; an Approve on top of it would run \
         the command against a card that records nobody allowing it"
    );
    assert_eq!(
        f.resolutions(number).await,
        vec![(
            "call-1".to_owned(),
            ApprovalDecision::Deny,
            ApprovalResolution::Abandoned
        )],
        "and one closer wrote one resolution"
    );
}

#[tokio::test]
async fn close_card_prompts_only_touches_its_own_card() {
    let f = fixture().await;
    let (first, first_session) = f.card("First").await;
    let (second, second_session) = f.card("Second").await;
    let gate = f.gate(Arc::new(ParkedGate));

    let a = tokio::spawn({
        let gate = Arc::clone(&gate);
        async move { gate.request(request(&first_session, "call-a")).await }
    });
    let b = tokio::spawn({
        let gate = Arc::clone(&gate);
        async move { gate.request(request(&second_session, "call-b")).await }
    });
    wait_for_announcement(&f, first).await;
    wait_for_announcement(&f, second).await;

    gate.close_card_prompts(&f.project, first).await;

    assert_eq!(f.resolutions(first).await.len(), 1, "its own card closed");
    assert!(
        f.resolutions(second).await.is_empty(),
        "another card's prompt is still live"
    );

    a.abort();
    b.abort();
    let _ = a.await;
    let _ = b.await;
}
