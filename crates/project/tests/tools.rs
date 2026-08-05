//! The board's tools, exercised through the same `Tool::execute` the agent
//! loop calls.
//!
//! The through-line: an agent may only reach the project its session
//! belongs to, and it addresses everything by the names a person reads off
//! the board — `#4`, `@dev-1` — never by ULID.

use std::sync::Arc;

use baybo_model::{IssueId, ProjectId};
use baybo_project::{NewProject, ProjectManager};
use baybo_tools::{Tool, ToolContext, ToolError, ToolManifest, ToolOutput, ToolTriggerScope};
use baybo_workspace::WorkspacePaths;
use serde_json::{Value, json};

struct Fixture {
    manager: Arc<ProjectManager>,
    tools: Vec<(Arc<dyn Tool>, ToolManifest)>,
    paths: WorkspacePaths,
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
        paths.clone(),
        Arc::new(baybo_project::NoopProjectEvents),
        baybo_project::no_dispatch(),
    ));
    Fixture {
        tools: baybo_project::tools::agent_tools(Arc::clone(&manager)),
        manager,
        paths,
        _workspace: workspace,
    }
}

impl Fixture {
    fn tool(&self, name: &str) -> &Arc<dyn Tool> {
        &self
            .tools
            .iter()
            .find(|(tool, _)| tool.name() == name)
            .unwrap_or_else(|| panic!("no {name} tool"))
            .0
    }

    /// A context that looks like an issue run on `project`.
    fn ctx(&self, project: &ProjectId, agent: &baybo_model::AgentProfileId) -> ToolContext {
        ToolContext {
            session_trigger: baybo_model::TriggerSource::Issue {
                project_id: project.clone(),
                issue_id: IssueId::generate(),
                number: 1,
            },
            agent_id: agent.clone(),
            workspace_paths: self.paths.clone(),
            ..ToolContext::for_test()
        }
    }

    async fn call(&self, name: &str, ctx: &ToolContext, params: Value) -> Value {
        match self.tool(name).execute(params, ctx).await {
            Ok(ToolOutput::Json(v)) => v,
            other => panic!("{name} did not return json: {other:?}"),
        }
    }

    async fn refuse(&self, name: &str, ctx: &ToolContext, params: Value) -> String {
        match self.tool(name).execute(params, ctx).await {
            Err(e) => e.to_string(),
            Ok(out) => panic!("{name} should have refused, got {out:?}"),
        }
    }

    async fn open(&self, name: &str) -> (ProjectId, baybo_model::AgentProfileId) {
        let project = self
            .manager
            .create_project(NewProject {
                name: name.to_owned(),
                description: String::new(),
                workdir: None,
                daily_budget: None,
            })
            .await
            .expect("create project");
        let lead = self.manager.team(&project.id).await.expect("team")[0]
            .id
            .clone();
        (project.id, lead)
    }
}

/// The scope check is the security model here: a tool that took a
/// `project_id` would let one board's agent edit another's.
#[tokio::test]
async fn every_board_tool_is_scoped_to_its_own_session() {
    let f = fixture().await;
    let (mine, lead) = f.open("mine").await;
    let (theirs, _) = f.open("theirs").await;
    f.manager
        .create_issue(
            &theirs,
            baybo_store::project::IssueActor::User,
            baybo_project::NewIssueRequest {
                title: "somebody else's card".to_owned(),
                description: String::new(),
                status: baybo_store::project::IssueStatus::Backlog,
                priority: baybo_store::project::IssuePriority::None,
                assignee: None,
                parent: None,
                stage: 0,
            },
        )
        .await
        .expect("their issue");

    let ctx = f.ctx(&mine, &lead);
    // Nothing on the other board is visible…
    let listed = f.call("IssueList", &ctx, json!({})).await;
    assert_eq!(listed["count"], 0);
    // …and its #1 is simply not there, rather than reachable by number.
    let err = f.refuse("IssueGet", &ctx, json!({ "number": 1 })).await;
    assert!(err.contains("issue #1"), "{err}");

    // Every one of them declares the board scope, so none is ever offered
    // to a session that has no project.
    for (tool, _) in &f.tools {
        assert_eq!(
            tool.trigger_scope(),
            ToolTriggerScope::ProjectBoard,
            "{} must be board-scoped",
            tool.name()
        );
    }
    // And a call that reaches one anyway fails closed rather than guessing.
    let stray = ToolContext::for_test();
    for (tool, _) in &f.tools {
        let err = tool
            .execute(
                json!({ "number": 1, "text": "x", "title": "x", "name": "x", "role": "x" }),
                &stray,
            )
            .await
            .expect_err(tool.name());
        assert!(
            err.to_string().contains("does not belong to a project"),
            "{}: {err}",
            tool.name()
        );
    }
}

#[tokio::test]
async fn an_agent_opens_assigns_and_starts_work_by_handle() {
    let f = fixture().await;
    let (project, lead) = f.open("Work").await;
    let ctx = f.ctx(&project, &lead);

    let hired = f
        .call(
            "ProjectAgentCreate",
            &ctx,
            json!({ "name": "Test Engineer", "role": "Writes the tests." }),
        )
        .await;
    assert_eq!(hired["handle"], "@test-engineer");

    let created = f
        .call(
            "IssueCreate",
            &ctx,
            json!({
                "title": "Cover the parser",
                "description": "It has no tests at all.",
                "priority": "high",
            }),
        )
        .await;
    assert_eq!(created["number"], 1);
    assert_eq!(created["status"], "backlog");
    assert!(created.get("assignee").is_none());

    // Assign and start in one call: the fields land before the column does,
    // so the card arrives in In Progress already staffed.
    let started = f
        .call(
            "IssueUpdate",
            &ctx,
            json!({ "number": 1, "assignee": "@test-engineer", "status": "in_progress" }),
        )
        .await;
    assert_eq!(started["status"], "in_progress");
    assert_eq!(started["assignee"], "@test-engineer");
    assert_eq!(
        f.manager.active_runs(&project).await.expect("runs").len(),
        1,
        "a card reaching In Progress with somebody on it starts them"
    );

    // The hire's own record names who hired it.
    let team = f.manager.team(&project).await.expect("team");
    let hire = team
        .iter()
        .find(|row| {
            row.team
                .as_ref()
                .is_some_and(|t| t.handle.as_str() == "test-engineer")
        })
        .expect("the hire is on the team");
    assert_eq!(hire.hired_by.as_ref(), Some(&lead));
}

#[tokio::test]
async fn a_handle_that_is_not_on_this_board_does_not_resolve() {
    let f = fixture().await;
    let (mine, lead) = f.open("mine").await;
    let (theirs, _) = f.open("theirs").await;
    f.manager
        .hire(&theirs, member("Outsider"), None)
        .await
        .expect("hire");

    let ctx = f.ctx(&mine, &lead);
    f.call("IssueCreate", &ctx, json!({ "title": "work" }))
        .await;
    for handle in ["@outsider", "@nobody"] {
        let err = f
            .refuse(
                "IssueUpdate",
                &ctx,
                json!({ "number": 1, "assignee": handle }),
            )
            .await;
        assert!(err.contains("no agent"), "{handle}: {err}");
    }
}

#[tokio::test]
async fn a_comment_says_whether_anybody_will_read_it() {
    let f = fixture().await;
    let (project, lead) = f.open("Talking").await;
    let ctx = f.ctx(&project, &lead);
    f.call(
        "ProjectAgentCreate",
        &ctx,
        json!({ "name": "Dev", "role": "Codes." }),
    )
    .await;
    f.call("IssueCreate", &ctx, json!({ "title": "unowned" }))
        .await;

    // Nobody on it: the comment is history and says so, rather than leaving
    // the caller waiting for an answer that is not coming.
    let recorded = f
        .call(
            "IssueComment",
            &ctx,
            json!({ "number": 1, "text": "somebody should look at this" }),
        )
        .await;
    assert!(
        recorded["delivery"]
            .as_str()
            .expect("delivery")
            .contains("recorded only"),
        "{recorded}"
    );

    // Assigned and idle in a live column: the comment wakes them.
    f.call(
        "IssueUpdate",
        &ctx,
        json!({ "number": 1, "assignee": "@dev", "status": "todo" }),
    )
    .await;
    let delivered = f
        .call(
            "IssueComment",
            &ctx,
            json!({ "number": 1, "text": "start with the parser" }),
        )
        .await;
    assert!(
        delivered["delivery"]
            .as_str()
            .expect("delivery")
            .contains("woken"),
        "{delivered}"
    );
    assert_eq!(
        f.manager.active_runs(&project).await.expect("runs").len(),
        1
    );

    // …and the next one lands in the run that is already queued rather than
    // starting a second agent on one card.
    let merged = f
        .call(
            "IssueComment",
            &ctx,
            json!({ "number": 1, "text": "and then the lexer" }),
        )
        .await;
    assert!(
        merged["delivery"]
            .as_str()
            .expect("delivery")
            .contains("already queued"),
        "{merged}"
    );
    assert_eq!(
        f.manager.active_runs(&project).await.expect("runs").len(),
        1
    );
}

#[tokio::test]
async fn a_read_returns_the_card_and_what_has_been_said_on_it() {
    let f = fixture().await;
    let (project, lead) = f.open("Reading").await;
    let ctx = f.ctx(&project, &lead);
    f.call(
        "IssueCreate",
        &ctx,
        json!({ "title": "Fix the leak", "description": "Under load." }),
    )
    .await;
    f.call(
        "IssueComment",
        &ctx,
        json!({ "number": 1, "text": "reproduced it" }),
    )
    .await;

    let issue = f.call("IssueGet", &ctx, json!({ "number": 1 })).await;
    assert_eq!(issue["title"], "Fix the leak");
    assert_eq!(issue["description"], "Under load.");
    let timeline = issue["timeline"].as_array().expect("timeline");
    // Third person, by handle — the same sentences the operator reads.
    assert_eq!(timeline[0]["event"], "opened the issue");
    assert_eq!(timeline[0]["by"], "@lead");
    assert_eq!(timeline.last().expect("entry")["event"], "reproduced it");
}

/// Triage is "what has nobody picked up", so it has to be one filter rather
/// than a full board read the model sorts itself.
#[tokio::test]
async fn the_triage_filter_finds_the_cards_nobody_is_on() {
    let f = fixture().await;
    let (project, lead) = f.open("Triage").await;
    let ctx = f.ctx(&project, &lead);
    f.call(
        "ProjectAgentCreate",
        &ctx,
        json!({ "name": "Dev", "role": "Codes." }),
    )
    .await;
    for (title, priority) in [("low one", "low"), ("urgent one", "urgent")] {
        f.call(
            "IssueCreate",
            &ctx,
            json!({ "title": title, "priority": priority }),
        )
        .await;
    }
    f.call(
        "IssueCreate",
        &ctx,
        json!({ "title": "taken", "assignee": "@dev" }),
    )
    .await;

    let listed = f
        .call("IssueList", &ctx, json!({ "assignee": "unassigned" }))
        .await;
    assert_eq!(listed["count"], 2);
    let issues = listed["issues"].as_array().expect("issues");
    // Most urgent first, so the read is already in triage order.
    assert_eq!(issues[0]["title"], "urgent one");
    assert_eq!(issues[1]["title"], "low one");
    // The roster rides along, so deciding who to assign needs no second call.
    let team: Vec<&str> = listed["team"]
        .as_array()
        .expect("team")
        .iter()
        .map(|m| m["handle"].as_str().expect("handle"))
        .collect();
    assert_eq!(team, vec!["@dev", "@lead"]);

    let taken = f
        .call("IssueList", &ctx, json!({ "assignee": "@dev" }))
        .await;
    assert_eq!(taken["count"], 1);
    assert_eq!(taken["issues"][0]["title"], "taken");
}

#[tokio::test]
async fn an_update_that_changes_nothing_is_a_mistake_worth_saying() {
    let f = fixture().await;
    let (project, lead) = f.open("Empty").await;
    let ctx = f.ctx(&project, &lead);
    f.call("IssueCreate", &ctx, json!({ "title": "a card" }))
        .await;

    let err = f.refuse("IssueUpdate", &ctx, json!({ "number": 1 })).await;
    assert!(err.contains("at least one field"), "{err}");
    // A status equal to the one it already has is not a move, and must not
    // renumber the column or record a timeline entry for a non-event.
    let same = f
        .call(
            "IssueUpdate",
            &ctx,
            json!({ "number": 1, "status": "backlog" }),
        )
        .await;
    assert_eq!(same["status"], "backlog");
    let timeline = f.call("IssueGet", &ctx, json!({ "number": 1 })).await;
    assert_eq!(
        timeline["timeline"].as_array().expect("timeline").len(),
        1,
        "only the open entry"
    );
}

/// Every refusal an agent can provoke has to come back as `InvalidParams`,
/// which is what the loop shows the model as a correctable mistake — an
/// `Execution` error reads as the system breaking.
#[tokio::test]
async fn a_refusal_the_agent_can_fix_comes_back_as_bad_parameters() {
    let f = fixture().await;
    let (project, lead) = f.open("Refusals").await;
    let ctx = f.ctx(&project, &lead);

    for (name, params) in [
        ("IssueCreate", json!({ "title": "  " })),
        (
            "IssueCreate",
            json!({ "title": "unstaffed", "status": "in_progress" }),
        ),
        ("IssueGet", json!({ "number": 99 })),
        ("IssueComment", json!({ "number": 99, "text": "hello" })),
        ("ProjectAgentCreate", json!({ "name": "!!!", "role": "x" })),
    ] {
        let err = f
            .tool(name)
            .execute(params.clone(), &ctx)
            .await
            .expect_err(&format!("{name} {params}"));
        assert!(
            matches!(err, ToolError::InvalidParams(_)),
            "{name} {params}: {err:?}"
        );
    }
}

fn member(name: &str) -> baybo_project::NewTeamMember {
    baybo_project::NewTeamMember {
        name: name.to_owned(),
        role: "does things".to_owned(),
        framework: None,
        llm: None,
    }
}

/// The gate's own behaviour: it records both halves and stays out of the
/// way of the decision itself.
mod approvals {
    use super::*;
    use baybo_model::{ApprovalDecision, SessionId, TriggerSource};
    use baybo_store::project::IssueEventBody;
    use baybo_tools::{ApprovalGate, ApprovalRequest};

    /// A gate that answers with whatever it was built with, and remembers
    /// that it was asked — so the decorator can be shown to delegate
    /// rather than decide.
    struct FixedGate {
        decision: ApprovalDecision,
        asked: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ApprovalGate for FixedGate {
        async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
            self.asked.lock().push(req.call_id);
            self.decision
        }
    }

    fn request(session: &SessionId, call_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            call_id: call_id.to_owned(),
            tool_call_id: None,
            session_id: session.clone(),
            user_id: "owner".to_owned(),
            tool: "Bash".to_owned(),
            accesses: Vec::new(),
            params_preview: "{\"command\":\"rm -rf build\"}".to_owned(),
            description: Some("Clean the build directory".to_owned()),
        }
    }

    /// Bind a session to an issue the way an issue run's session is bound.
    async fn issue_session(
        store: &baybo_storage::Store,
        project: &ProjectId,
        number: i64,
        issue_id: IssueId,
    ) -> SessionId {
        let now = chrono::Utc::now();
        let id = SessionId::new();
        let session = baybo_model::Session {
            id: id.clone(),
            user: baybo_model::User {
                id: "owner".to_owned(),
                name: None,
                channel: baybo_model::ChannelType::owner(),
            },
            channel: baybo_model::ChannelType::owner(),
            created_at: now,
            last_active: now,
            state: baybo_model::SessionState::default(),
            root_session_id: id.clone(),
            trigger: TriggerSource::Issue {
                project_id: project.clone(),
                issue_id,
                number,
            },
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        };
        baybo_store::SessionStore::save(store.session.as_ref(), &session)
            .await
            .expect("create session");
        id
    }

    #[tokio::test]
    async fn a_prompt_from_a_run_lands_on_its_card_with_the_answer() {
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
            paths,
            Arc::new(baybo_project::NoopProjectEvents),
            baybo_project::no_dispatch(),
        ));
        let project = manager
            .create_project(NewProject {
                name: "Approving".to_owned(),
                description: String::new(),
                workdir: None,
                daily_budget: None,
            })
            .await
            .expect("project");
        let lead = manager.team(&project.id).await.expect("team")[0].id.clone();
        let issue = manager
            .create_issue(
                &project.id,
                baybo_store::project::IssueActor::User,
                baybo_project::NewIssueRequest {
                    title: "needs a hand".to_owned(),
                    description: String::new(),
                    status: baybo_store::project::IssueStatus::InProgress,
                    priority: baybo_store::project::IssuePriority::None,
                    assignee: Some(lead.clone()),
                    parent: None,
                    stage: 0,
                },
            )
            .await
            .expect("issue");

        let asked: Arc<parking_lot::Mutex<Vec<String>>> = Arc::default();
        let gate = baybo_project::TimelineApprovalGate::new(
            Arc::new(FixedGate {
                decision: ApprovalDecision::Deny,
                asked: Arc::clone(&asked),
            }),
            Arc::clone(&manager),
            Arc::clone(&store.session),
        );

        let session = issue_session(&store, &project.id, issue.number, issue.id.clone()).await;
        let decision = gate.request(request(&session, "c1")).await;

        // The wrapper observes; it never decides.
        assert_eq!(decision, ApprovalDecision::Deny);
        assert_eq!(*asked.lock(), vec!["c1".to_owned()]);

        let timeline = manager
            .timeline(&project.id, issue.number)
            .await
            .expect("timeline");
        let requested = timeline
            .iter()
            .find_map(|e| match &e.body {
                IssueEventBody::ApprovalRequested {
                    call_id,
                    tool,
                    summary,
                } => Some((
                    call_id.clone(),
                    tool.clone(),
                    summary.clone(),
                    e.actor.clone(),
                )),
                _ => None,
            })
            .expect("the prompt is on the card");
        assert_eq!(requested.0, "c1");
        assert_eq!(requested.1, "Bash");
        assert_eq!(requested.2, "Clean the build directory");
        assert_eq!(
            requested.3,
            baybo_store::project::IssueActor::Agent(lead),
            "a prompt is something the agent asked for, not something the operator did"
        );
        // Both halves, so the card never stops explaining itself at the
        // prompt — including on the gate's own deny-on-timeout.
        assert!(timeline.iter().any(|e| matches!(
            &e.body,
            IssueEventBody::ApprovalResolved { call_id, decision }
                if call_id == "c1" && *decision == ApprovalDecision::Deny
        )));
        assert!(baybo_project::pending_approvals(&timeline).is_empty());
    }

    /// An ordinary conversation's prompt must pass straight through: the
    /// gate is shared by every session on the channel.
    #[tokio::test]
    async fn a_prompt_from_an_ordinary_session_is_only_forwarded() {
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
            paths,
            Arc::new(baybo_project::NoopProjectEvents),
            baybo_project::no_dispatch(),
        ));
        let asked: Arc<parking_lot::Mutex<Vec<String>>> = Arc::default();
        let gate = baybo_project::TimelineApprovalGate::new(
            Arc::new(FixedGate {
                decision: ApprovalDecision::Approve,
                asked: Arc::clone(&asked),
            }),
            manager,
            Arc::clone(&store.session),
        );

        // A session that was never created, let alone bound to an issue —
        // the shape a chat prompt arrives in as far as this gate is
        // concerned.
        let decision = gate
            .request(request(&SessionId::from("not-an-issue".to_owned()), "c9"))
            .await;
        assert_eq!(decision, ApprovalDecision::Approve);
        assert_eq!(*asked.lock(), vec!["c9".to_owned()]);
    }
}
