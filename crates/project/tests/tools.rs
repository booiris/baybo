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
        Arc::clone(&store.blob),
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

    /// A board that starts only what a tool call asks it to: these tests are
    /// about the tools, and the driver would otherwise promote their cards
    /// out of Todo between one assertion and the next.
    async fn open(&self, name: &str) -> (ProjectId, baybo_model::AgentProfileId) {
        let project = self
            .manager
            .create_project(NewProject {
                name: name.to_owned(),
                description: String::new(),
                workdir: None,
                daily_budget: None,
                daily_budget_tokens: None,
                max_parallel_issue_runs: Some(0),
            })
            .await
            .expect("create project");
        let lead = self.manager.team(&project.id).await.expect("team")[0]
            .id
            .clone();
        (project.id, lead)
    }
}

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
                attachments: Vec::new(),
                status: baybo_store::project::IssueStatus::Backlog,
                priority: baybo_store::project::IssuePriority::None,
                assignee: None,
                parent: None,
                stage: 0,
                source_key: None,
            },
        )
        .await
        .expect("their issue")
        .into_issue();

    let ctx = f.ctx(&mine, &lead);
    let listed = f.call("IssueList", &ctx, json!({})).await;
    assert_eq!(listed["count"], 0);
    let err = f.refuse("IssueGet", &ctx, json!({ "number": 1 })).await;
    assert!(err.contains("issue #1"), "{err}");

    for (tool, _) in &f.tools {
        assert_eq!(
            tool.trigger_scope(),
            ToolTriggerScope::ProjectBoard,
            "{} must be board-scoped",
            tool.name()
        );
    }
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
            json!({ "name": "test-engineer", "role": "Writes the tests." }),
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
        .hire(&theirs, member("outsider"), None)
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
        json!({ "name": "dev", "role": "Codes." }),
    )
    .await;
    f.call("IssueCreate", &ctx, json!({ "title": "unowned" }))
        .await;

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
    assert_eq!(timeline[0]["event"], "opened the issue");
    assert_eq!(timeline[0]["by"], "@lead");
    assert_eq!(timeline.last().expect("entry")["event"], "reproduced it");
}

#[tokio::test]
async fn the_triage_filter_finds_the_cards_nobody_is_on() {
    let f = fixture().await;
    let (project, lead) = f.open("Triage").await;
    let ctx = f.ctx(&project, &lead);
    f.call(
        "ProjectAgentCreate",
        &ctx,
        json!({ "name": "dev", "role": "Codes." }),
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
    assert_eq!(issues[0]["title"], "urgent one");
    assert_eq!(issues[1]["title"], "low one");
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
async fn a_departed_teammate_is_still_named_on_the_timeline_an_agent_reads() {
    let f = fixture().await;
    let (project, lead) = f.open("Turnover").await;
    let ctx = f.ctx(&project, &lead);
    let hired = f
        .call(
            "ProjectAgentCreate",
            &ctx,
            json!({ "name": "test-engineer", "role": "Writes the tests." }),
        )
        .await;
    assert_eq!(hired["handle"], "@test-engineer");
    f.call("IssueCreate", &ctx, json!({ "title": "Cover the parser" }))
        .await;
    f.call(
        "IssueUpdate",
        &ctx,
        json!({ "number": 1, "assignee": "@test-engineer" }),
    )
    .await;

    let leaver = f
        .manager
        .team(&project)
        .await
        .expect("team")
        .into_iter()
        .find(|row| {
            row.team
                .as_ref()
                .is_some_and(|t| t.handle.as_str() == "test-engineer")
        })
        .expect("the hire is on the roster")
        .id;
    f.manager
        .remove_from_team(&project, &leaver)
        .await
        .expect("it leaves the team");
    assert!(
        !f.manager
            .team(&project)
            .await
            .expect("team")
            .iter()
            .any(|row| row.id == leaver),
        "the roster has stopped carrying it, which is the whole problem"
    );

    let card = f.call("IssueGet", &ctx, json!({ "number": 1 })).await;
    let rendered = card.to_string();
    assert!(
        !rendered.contains(leaver.as_str()),
        "no ULID may reach an agent's reader: {rendered}"
    );
    assert_eq!(card["assignee"], "@test-engineer");
    let assigned = card["timeline"]
        .as_array()
        .expect("timeline")
        .iter()
        .filter_map(|entry| entry["event"].as_str())
        .find(|event| event.starts_with("assigned it to"))
        .expect("the assignment is on the timeline");
    assert_eq!(assigned, "assigned it to @test-engineer");
}

#[tokio::test]
async fn a_timeline_never_renders_another_boards_handle() {
    let f = fixture().await;
    let (mine, lead) = f.open("mine").await;
    let (_theirs, their_lead) = f.open("theirs").await;
    let ctx = f.ctx(&mine, &lead);
    f.call("IssueCreate", &ctx, json!({ "title": "our card" }))
        .await;

    f.manager
        .record_event(
            &mine,
            1,
            baybo_store::project::IssueActor::Agent(their_lead.clone()),
            baybo_store::project::IssueEventBody::Comment {
                text: "passing through".to_owned(),
                attachments: Vec::new(),
            },
        )
        .await;

    let card = f.call("IssueGet", &ctx, json!({ "number": 1 })).await;
    let timeline = card["timeline"].as_array().expect("timeline").clone();
    let foreign = timeline
        .iter()
        .find(|entry| entry["event"] == "passing through")
        .expect("the entry is on the card");
    assert_eq!(
        foreign["by"],
        json!(their_lead.as_str()),
        "a foreign id renders as itself, not under this board's naming"
    );
    assert!(
        timeline
            .iter()
            .any(|entry| entry["event"] == "opened the issue" && entry["by"] == json!("@lead")),
        "{timeline:?}"
    );
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

mod approvals {
    use super::*;
    use baybo_model::{ApprovalDecision, SessionId, TriggerSource};
    use baybo_store::project::IssueEventBody;
    use baybo_tools::{ApprovalGate, ApprovalRequest};

    struct FixedGate {
        decision: ApprovalDecision,
        asked: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ApprovalGate for FixedGate {
        async fn request(&self, req: ApprovalRequest) -> baybo_tools::ApprovalOutcome {
            self.asked.lock().push(req.call_id);
            baybo_tools::ApprovalOutcome::answered(self.decision)
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
            Arc::clone(&store.blob),
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
                daily_budget_tokens: None,
                max_parallel_issue_runs: None,
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
                    attachments: Vec::new(),
                    status: baybo_store::project::IssueStatus::InProgress,
                    priority: baybo_store::project::IssuePriority::None,
                    assignee: Some(lead.clone()),
                    parent: None,
                    stage: 0,
                    source_key: None,
                },
            )
            .await
            .expect("issue")
            .into_issue();

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
        let decision = gate.request(request(&session, "c1")).await.decision;

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
                    ..
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
        assert!(timeline.iter().any(|e| matches!(
            &e.body,
            IssueEventBody::ApprovalResolved { call_id, decision, resolution }
                if call_id == "c1"
                    && *decision == ApprovalDecision::Deny
                    && *resolution == baybo_model::ApprovalResolution::Answered
        )));
    }

    /// An inner gate that never answers — the shape of a prompt still parked
    /// when its run is torn down.
    struct ParkedGate;

    #[async_trait::async_trait]
    impl ApprovalGate for ParkedGate {
        async fn request(&self, _req: ApprovalRequest) -> baybo_tools::ApprovalOutcome {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_prompt_dropped_undecided_closes_on_the_card_as_abandoned() {
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
                name: "Abandoning".to_owned(),
                description: String::new(),
                workdir: None,
                daily_budget: None,
                daily_budget_tokens: None,
                max_parallel_issue_runs: None,
            })
            .await
            .expect("project");
        let lead = manager.team(&project.id).await.expect("team")[0].id.clone();
        let issue = manager
            .create_issue(
                &project.id,
                baybo_store::project::IssueActor::User,
                baybo_project::NewIssueRequest {
                    title: "left waiting".to_owned(),
                    description: String::new(),
                    attachments: Vec::new(),
                    status: baybo_store::project::IssueStatus::InProgress,
                    priority: baybo_store::project::IssuePriority::None,
                    assignee: Some(lead),
                    parent: None,
                    stage: 0,
                    source_key: None,
                },
            )
            .await
            .expect("issue")
            .into_issue();

        let gate = baybo_project::TimelineApprovalGate::new(
            Arc::new(ParkedGate),
            Arc::clone(&manager),
            Arc::clone(&store.session),
        );
        let session = issue_session(&store, &project.id, issue.number, issue.id.clone()).await;

        let parked = tokio::spawn({
            let req = request(&session, "c7");
            async move { gate.request(req).await }
        });
        let requested = |timeline: &[baybo_store::project::IssueEventRow]| {
            timeline.iter().any(|e| {
                matches!(&e.body, IssueEventBody::ApprovalRequested { call_id, .. } if call_id == "c7")
            })
        };
        for _ in 0..200 {
            let timeline = manager
                .timeline(&project.id, issue.number)
                .await
                .expect("timeline");
            if requested(&timeline) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // The run dies with the prompt still up: the request future is
        // dropped undecided.
        parked.abort();
        let _ = parked.await;

        let mut closed = false;
        for _ in 0..200 {
            let timeline = manager
                .timeline(&project.id, issue.number)
                .await
                .expect("timeline");
            closed = timeline.iter().any(|e| {
                matches!(
                    &e.body,
                    IssueEventBody::ApprovalResolved { call_id, decision, resolution }
                        if call_id == "c7"
                            && *decision == ApprovalDecision::Deny
                            && *resolution == baybo_model::ApprovalResolution::Abandoned
                )
            });
            if closed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            closed,
            "a dropped prompt still closes its ledger entry, as abandoned"
        );
    }

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
            Arc::clone(&store.blob),
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

        let decision = gate
            .request(request(&SessionId::from("not-an-issue".to_owned()), "c9"))
            .await
            .decision;
        assert_eq!(decision, ApprovalDecision::Approve);
        assert_eq!(*asked.lock(), vec!["c9".to_owned()]);
    }
}

#[tokio::test]
async fn the_triage_read_says_who_is_free_and_what_is_stuck() {
    let f = fixture().await;
    let (project, lead) = f.open("Triage Facts").await;
    let ctx = f.ctx(&project, &lead);
    f.call(
        "ProjectAgentCreate",
        &ctx,
        json!({ "name": "dev", "role": "Codes." }),
    )
    .await;

    f.call(
        "IssueCreate",
        &ctx,
        json!({ "title": "in flight", "assignee": "@dev", "status": "in_progress" }),
    )
    .await;
    f.call("IssueCreate", &ctx, json!({ "title": "waiting" }))
        .await;

    let listed = f
        .call("IssueList", &ctx, json!({ "assignee": "unassigned" }))
        .await;
    assert_eq!(listed["count"], 1, "the filter still narrows the rows");

    let team = listed["team"].as_array().expect("team");
    let dev = team
        .iter()
        .find(|m| m["handle"] == "@dev")
        .expect("dev is on the roster");
    assert_eq!(
        dev["working_on"].as_array().expect("working_on"),
        &vec![json!(1)],
        "load is derived from runs over the whole board, not from the filtered rows"
    );
    let lead_row = team
        .iter()
        .find(|m| m["handle"] == "@lead")
        .expect("the lead is on the roster");
    assert_eq!(lead_row["lead"], true);
    assert_eq!(
        lead_row["you"], true,
        "the caller needs its own handle to assign work to itself"
    );
    assert!(lead_row.get("working_on").is_none(), "nothing in flight");
}

#[tokio::test]
async fn an_exhausted_board_reads_as_idle_and_says_why() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            name: "Skint".to_owned(),
            description: String::new(),
            workdir: None,
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            daily_budget_tokens: None,
            max_parallel_issue_runs: None,
        })
        .await
        .expect("project");
    let lead = f.manager.team(&project.id).await.expect("team")[0]
        .id
        .clone();
    let ctx = f.ctx(&project.id, &lead);
    f.call(
        "IssueCreate",
        &ctx,
        json!({ "title": "held work", "assignee": "@lead", "status": "in_progress" }),
    )
    .await;

    let listed = f.call("IssueList", &ctx, json!({})).await;
    let lead_row = listed["team"]
        .as_array()
        .expect("team")
        .iter()
        .find(|m| m["handle"] == "@lead")
        .expect("lead");
    assert!(
        lead_row.get("working_on").is_none(),
        "a held run is not somebody working: {lead_row}"
    );
    assert_eq!(
        listed["board"]["held"].as_array().expect("held"),
        &vec![json!(1)]
    );
    assert_eq!(listed["board"]["budget"]["exhausted"], true);
    assert_eq!(listed["board"]["budget"]["limit"], "$0.00");
}

#[tokio::test]
async fn a_board_held_on_tokens_reports_tokens_not_dollars() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            name: "Token plan".to_owned(),
            description: String::new(),
            workdir: None,
            daily_budget: None,
            daily_budget_tokens: Some(0),
            max_parallel_issue_runs: None,
        })
        .await
        .expect("project");
    let lead = f.manager.team(&project.id).await.expect("team")[0]
        .id
        .clone();
    let ctx = f.ctx(&project.id, &lead);
    f.call(
        "IssueCreate",
        &ctx,
        json!({ "title": "held work", "assignee": "@lead", "status": "in_progress" }),
    )
    .await;

    let listed = f.call("IssueList", &ctx, json!({})).await;
    let budget = &listed["board"]["budget"];
    assert_eq!(budget["exhausted"], true);
    assert_eq!(budget["limit"], "0 tokens");
    assert_eq!(budget["spent"], "0 tokens");
    assert!(
        !budget.to_string().contains('$'),
        "a board with no money ceiling must not be described in money: {budget}"
    );
}

#[tokio::test]
async fn a_board_with_no_ceiling_reports_no_budget() {
    let f = fixture().await;
    let (project, lead) = f.open("Unlimited").await;
    let ctx = f.ctx(&project, &lead);
    let listed = f.call("IssueList", &ctx, json!({})).await;
    assert!(
        listed["board"].get("budget").is_none(),
        "{}",
        listed["board"]
    );
    assert!(listed["board"].get("held").is_none());
}

#[tokio::test]
async fn a_parent_row_carries_its_progress_and_open_stages() {
    let f = fixture().await;
    let (project, lead) = f.open("Stages").await;
    let ctx = f.ctx(&project, &lead);
    f.call("IssueCreate", &ctx, json!({ "title": "the whole thing" }))
        .await;
    for (title, stage) in [("design", 0), ("build", 1)] {
        f.call(
            "IssueCreate",
            &ctx,
            json!({ "title": title, "parent": 1, "stage": stage }),
        )
        .await;
    }
    f.call(
        "IssueUpdate",
        &ctx,
        json!({ "number": 2, "status": "done" }),
    )
    .await;

    let listed = f
        .call("IssueList", &ctx, json!({ "status": "backlog" }))
        .await;
    let parent = listed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .find(|i| i["number"] == 1)
        .expect("the parent");
    assert_eq!(parent["sub_issues"]["done"], 1);
    assert_eq!(parent["sub_issues"]["total"], 2);
    assert_eq!(
        parent["sub_issues"]["open_stages"]
            .as_array()
            .expect("stages"),
        &vec![json!(1)],
        "stage 0 finished, so stage 1 is what is left"
    );
    assert!(parent.get("parent").is_none(), "a top-level card has none");

    let child = listed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .find(|i| i["number"] == 3)
        .expect("the step still in backlog");
    assert_eq!(child["parent"], 1);
    assert_eq!(child["stage"], 1);
}

mod dedupe {
    use super::*;
    use baybo_model::TriggerSource;

    fn fire_ctx(
        f: &Fixture,
        project: &ProjectId,
        agent: &baybo_model::AgentProfileId,
        job: &str,
    ) -> ToolContext {
        ToolContext {
            session_trigger: TriggerSource::Cron {
                cron_job_id: job.to_owned(),
                origin_session_id: None,
                conversation: true,
                job_title: Some("nightly check".to_owned()),
                project_id: Some(project.clone()),
            },
            agent_id: agent.clone(),
            workspace_paths: f.paths.clone(),
            ..ToolContext::for_test()
        }
    }

    #[tokio::test]
    async fn a_daily_check_keeps_one_open_card_without_being_asked() {
        let f = fixture().await;
        let (project, lead) = f.open("Nightly").await;
        let ctx = fire_ctx(&f, &project, &lead, "cj-1");

        let first = f
            .call("IssueCreate", &ctx, json!({ "title": "the build is red" }))
            .await;
        assert_eq!(first["number"], 1);
        assert!(first.get("already_open").is_none());

        let second = f
            .call("IssueCreate", &ctx, json!({ "title": "the build is red" }))
            .await;
        assert_eq!(second["number"], 1, "the same card, not a second one");
        assert_eq!(second["already_open"], true);
        assert_eq!(
            f.manager.list_issues(&project).await.expect("issues").len(),
            1
        );
    }

    #[tokio::test]
    async fn a_finished_card_releases_its_key_for_the_next_occurrence() {
        let f = fixture().await;
        let (project, lead) = f.open("Recurring").await;
        let ctx = fire_ctx(&f, &project, &lead, "cj-1");
        f.call(
            "IssueCreate",
            &ctx,
            json!({ "title": "this month's failure" }),
        )
        .await;
        f.call(
            "IssueUpdate",
            &ctx,
            json!({ "number": 1, "status": "done" }),
        )
        .await;

        let next = f
            .call(
                "IssueCreate",
                &ctx,
                json!({ "title": "next month's failure" }),
            )
            .await;
        assert_eq!(next["number"], 2);
        assert!(next.get("already_open").is_none());

        f.call(
            "IssueUpdate",
            &ctx,
            json!({ "number": 2, "cancelled": true }),
        )
        .await;
        let third = f
            .call("IssueCreate", &ctx, json!({ "title": "and again" }))
            .await;
        assert_eq!(third["number"], 3);
    }

    #[tokio::test]
    async fn a_suffix_lets_one_check_file_several_cards() {
        let f = fixture().await;
        let (project, lead) = f.open("Several").await;
        let ctx = fire_ctx(&f, &project, &lead, "cj-1");
        for suffix in ["parser", "lexer"] {
            f.call(
                "IssueCreate",
                &ctx,
                json!({ "title": format!("{suffix} is failing"), "key": suffix }),
            )
            .await;
        }
        assert_eq!(
            f.manager.list_issues(&project).await.expect("issues").len(),
            2
        );
        let again = f
            .call(
                "IssueCreate",
                &ctx,
                json!({ "title": "parser is failing", "key": "parser" }),
            )
            .await;
        assert_eq!(again["already_open"], true);
    }

    #[tokio::test]
    async fn two_jobs_on_one_board_do_not_collide() {
        let f = fixture().await;
        let (project, lead) = f.open("Two Jobs").await;
        f.call(
            "IssueCreate",
            &fire_ctx(&f, &project, &lead, "cj-1"),
            json!({ "title": "from job one" }),
        )
        .await;
        let second = f
            .call(
                "IssueCreate",
                &fire_ctx(&f, &project, &lead, "cj-2"),
                json!({ "title": "from job two" }),
            )
            .await;
        assert_eq!(second["number"], 2);
        assert!(second.get("already_open").is_none());
    }

    #[tokio::test]
    async fn an_ordinary_run_gets_no_key_and_so_never_dedupes() {
        let f = fixture().await;
        let (project, lead) = f.open("Runs").await;
        let ctx = f.ctx(&project, &lead);
        for _ in 0..2 {
            f.call("IssueCreate", &ctx, json!({ "title": "same title" }))
                .await;
        }
        assert_eq!(
            f.manager.list_issues(&project).await.expect("issues").len(),
            2
        );
    }
}
