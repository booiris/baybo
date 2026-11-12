//! A parked approval on an issue session reaches its card, and a settled run
//! takes its prompts with it — through the ask ledger, closed exactly once.

use std::sync::Arc;

use baybo_human_ask::test_support::{MemoryHumanAskStore, NoResume};
use baybo_human_ask::{AskCore, AskQueues, ChannelAskGate, HumanAskManager, HumanAskManagerConfig};
use baybo_model::human_ask::{AskId, AskResolution};
use baybo_model::{
    ApprovalDecision, ApprovalResolution, ChannelType, ProjectId, SessionId, TriggerSource, User,
};
use baybo_project::{NewIssueRequest, NewProject, ProjectManager};
use baybo_store::SessionStore;
use baybo_store::project::{
    IssueActor, IssueEventBody, IssuePriority, IssueRunRow, IssueStatus, RunStatus,
};
use baybo_tools::{ApprovalGate, ApprovalOutcome, ApprovalRequest, AskCancel, AskSite};
use baybo_workspace::WorkspacePaths;
use tokio::task::JoinHandle;

struct Fixture {
    manager: Arc<ProjectManager>,
    sessions: Arc<dyn SessionStore>,
    asks: Arc<HumanAskManager>,
    gate: Arc<dyn ApprovalGate>,
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
        baybo_project::no_stopper(),
    ));
    let project = manager
        .create_project(NewProject {
            name: "Approvals".to_owned(),
            description: String::new(),
            workdir: None,
            daily_budget: None,
            daily_budget_tokens: None,
            max_parallel_issue_runs: Some(0),
            agents_may_merge: false,
        })
        .await
        .expect("create project")
        .id;
    let sessions: Arc<dyn SessionStore> =
        Arc::new(baybo_session::test_support::MemorySessionStore::new());
    let core = Arc::new(AskCore::new(MemoryHumanAskStore::new()));
    let queues = AskQueues::new();
    let asks = Arc::new(HumanAskManager::from_config(HumanAskManagerConfig {
        core: Arc::clone(&core),
        queues: queues.clone(),
        resume_sink: Arc::new(NoResume),
    }));
    baybo_project::install_ask_bridge(&core, &manager, Arc::clone(&asks), Arc::clone(&sessions));
    let gate: Arc<dyn ApprovalGate> = Arc::new(ChannelAskGate::new(
        queues.get_or_create(&ChannelType::owner()),
        core,
    ));
    Fixture {
        manager,
        sessions,
        asks,
        gate,
        project,
        _workspace: workspace,
    }
}

impl Fixture {
    async fn card(&self, title: &str) -> (i64, SessionId) {
        self.open_card(title, IssueStatus::Backlog, None, None)
            .await
    }

    /// This board's `@lead`, which is who every coordination run executes as.
    async fn lead(&self) -> baybo_model::AgentProfileId {
        self.manager
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
            .id
    }

    async fn card_with_a_run(&self, title: &str) -> (i64, SessionId, IssueRunRow) {
        let lead = self.lead().await;
        let (number, session) = self
            .open_card(
                title,
                IssueStatus::InProgress,
                Some(lead.clone()),
                Some(lead),
            )
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

    /// `runner` is the agent the session is **bound** to, which is what the
    /// router does in production and is deliberately allowed to differ from
    /// the card's assignee: that is exactly the shape of a coordination run.
    async fn open_card(
        &self,
        title: &str,
        status: IssueStatus,
        assignee: Option<baybo_model::AgentProfileId>,
        runner: Option<baybo_model::AgentProfileId>,
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
                    filed_from: None,
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
                state: baybo_model::SessionState {
                    agent_id: runner,
                    ..Default::default()
                },
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

    async fn raise(&self, session_id: &SessionId, call_id: &str) -> JoinHandle<ApprovalOutcome> {
        let gate = Arc::clone(&self.gate);
        let req = request(session_id, call_id);
        tokio::spawn(async move {
            gate.request(req, AskSite::for_test(), AskCancel::never())
                .await
        })
    }

    async fn run_of(&self, number: i64) -> IssueRunRow {
        self.manager
            .list_runs(&self.project, number)
            .await
            .expect("runs")
            .into_iter()
            .next()
            .expect("a run")
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

fn new_member(name: &str) -> baybo_project::NewTeamMember {
    baybo_project::NewTeamMember {
        name: name.to_owned(),
        role: "Reviews what the others build.".to_owned(),
        framework: None,
        llm: baybo_model::LlmPin::unpinned(),
    }
}

fn request(session_id: &SessionId, call_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        call_id: call_id.to_owned(),
        tool_call_id: String::new(),
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

fn ask(call_id: &str) -> AskId {
    call_id.parse().expect("ask id")
}

async fn outcome_of(task: JoinHandle<ApprovalOutcome>) -> ApprovalOutcome {
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the request is un-parked")
        .expect("the request task")
}

#[tokio::test]
async fn an_answered_prompt_records_the_real_outcome_once() {
    let f = fixture().await;
    let (number, session_id) = f.card("Answered").await;
    let parked = f.raise(&session_id, "call-1").await;
    let call_id = wait_for_announcement(&f, number).await;
    assert_eq!(call_id, "call-1");

    f.asks
        .resolve(
            &ask("call-1"),
            AskResolution::Approval(ApprovalDecision::Approve),
        )
        .await
        .expect("resolve");
    assert_eq!(
        outcome_of(parked).await,
        ApprovalOutcome::answered(ApprovalDecision::Approve)
    );
    assert_eq!(
        f.resolutions(number).await,
        vec![(
            "call-1".to_owned(),
            ApprovalDecision::Approve,
            ApprovalResolution::Answered
        )]
    );
    assert!(
        f.asks
            .resolve(
                &ask("call-1"),
                AskResolution::Approval(ApprovalDecision::Deny)
            )
            .await
            .is_err(),
        "the second answer loses the claim"
    );
    assert_eq!(f.resolutions(number).await.len(), 1);
}

#[tokio::test]
async fn the_request_entry_carries_the_parked_runs_attempt() {
    let f = fixture().await;
    let (number, session_id, run) = f.card_with_a_run("Attempted").await;
    let parked = f.raise(&session_id, "call-1").await;
    wait_for_announcement(&f, number).await;
    let attempt = f
        .manager
        .timeline(&f.project, number)
        .await
        .expect("timeline")
        .into_iter()
        .find_map(|row| match row.body {
            IssueEventBody::ApprovalRequested { attempt, .. } => Some(attempt),
            _ => None,
        })
        .expect("requested");
    assert_eq!(attempt, Some(run.attempt));
    parked.abort();
}

#[tokio::test]
async fn a_settling_run_takes_its_own_parked_prompt_with_it() {
    let f = fixture().await;
    let (number, session_id, run) = f.card_with_a_run("Settles mid-prompt").await;
    assert!(
        f.manager
            .start_run(&run, &session_id)
            .await
            .expect("start run")
    );
    let run = f.run_of(number).await;
    let parked = f.raise(&session_id, "call-1").await;
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
        outcome_of(parked).await,
        ApprovalOutcome::abandoned(),
        "and the tool waiting on it is told, rather than parked against a card that says Deny"
    );
    assert!(
        f.asks
            .resolve(
                &ask("call-1"),
                AskResolution::Approval(ApprovalDecision::Approve)
            )
            .await
            .is_err(),
        "an Allow that races the closer loses the ledger claim"
    );
    assert_eq!(
        f.resolutions(number).await.len(),
        1,
        "one closer, one entry"
    );
}

#[tokio::test]
async fn closing_a_runs_prompts_only_touches_its_own_session() {
    let f = fixture().await;
    let (first, first_session, run) = f.card_with_a_run("First").await;
    let (second, second_session) = f.card("Second").await;
    assert!(
        f.manager
            .start_run(&run, &first_session)
            .await
            .expect("start run")
    );
    let run = f.run_of(first).await;
    let a = f.raise(&first_session, "call-a").await;
    let b = f.raise(&second_session, "call-b").await;
    wait_for_announcement(&f, first).await;
    wait_for_announcement(&f, second).await;

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

    assert_eq!(outcome_of(a).await, ApprovalOutcome::abandoned());
    assert_eq!(f.resolutions(first).await.len(), 1, "its own card closed");
    assert!(
        f.resolutions(second).await.is_empty(),
        "another card's prompt is still live"
    );
    b.abort();
}

#[tokio::test]
async fn a_shutdown_leaves_the_prompt_pending_on_the_card() {
    let f = fixture().await;
    let (number, session_id) = f.card("Parked").await;
    let parked = f.raise(&session_id, "call-1").await;
    wait_for_announcement(&f, number).await;

    parked.abort();
    let _ = parked.await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    assert!(
        f.resolutions(number).await.is_empty(),
        "a dropped waiter orphans the ask; the next process answers it"
    );
    let row = f.asks.get(&ask("call-1")).await.expect("get").expect("row");
    assert!(row.is_pending());
}

/// A prompt names the agent whose run raised it, and that is routinely not
/// the card's assignee.
///
/// The gate used to read `issue.assignee`. A coordination run — Review,
/// Stalled, Blocked, Triage — executes as the board's `@lead` by construction
/// (`driver::takes_a_lead_question` refuses a card the lead is already on),
/// and on this board roughly a third of all runs are those. So every prompt
/// one of them raised was announced on the card under the *assignee's* name:
/// "@dev-1 asked you to approve a Bash call" for a call @lead made. The
/// binding on the session is the answer that cannot drift — it is written
/// once, where the assignee moves under a live run whenever somebody is
/// handed the card.
#[tokio::test]
async fn a_prompt_is_named_for_the_run_that_raised_it_not_the_cards_assignee() {
    let f = fixture().await;
    let lead = f.lead().await;
    let dev = f
        .manager
        .hire(&f.project, new_member("dev-1"), None)
        .await
        .expect("hire");

    // The card is @dev-1's; the run is the lead's, as a Review wake is.
    let (number, session) = f
        .open_card(
            "Reviewed by somebody else",
            IssueStatus::Review,
            Some(dev.id.clone()),
            Some(lead.clone()),
        )
        .await;

    let parked = f.raise(&session, "call-1").await;
    wait_for_announcement(&f, number).await;

    let actor = f
        .manager
        .timeline(&f.project, number)
        .await
        .expect("timeline")
        .into_iter()
        .find_map(|row| match row.body {
            IssueEventBody::ApprovalRequested { .. } => Some(row.actor),
            _ => None,
        })
        .expect("the prompt is on the card");
    assert_eq!(
        actor,
        IssueActor::Agent(lead),
        "the prompt belongs to the run that raised it"
    );
    assert_ne!(
        actor,
        IssueActor::Agent(dev.id),
        "and never to whoever happens to be staffed on the card"
    );
    parked.abort();
}
