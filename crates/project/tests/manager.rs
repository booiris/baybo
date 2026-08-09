use std::sync::Arc;

use baybo_model::{
    AgentFramework, AgentHandle, AgentProfileId, IssueId, IssueRunId, ProjectId, SessionId,
    TeamMembership,
};
use baybo_project::{NewIssueRequest, NewProject, ProjectError, ProjectManager};
use baybo_store::project::{
    AttentionCounts, IssueActor, IssueEventRow, IssuePriority, IssueRow, IssueRunRow, IssueStatus,
    IssueUpdate, NewIssue, NewIssueEvent, NewIssueRun, ProjectRow, ProjectUpdate,
    Result as StoreResult, RunStatus, RunTrigger,
};
use baybo_workspace::WorkspacePaths;
use chrono::{DateTime, Utc};

struct Fixture {
    manager: ProjectManager,
    store: Arc<dyn baybo_store::project::ProjectStore>,
    dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>>,
    agents: Arc<dyn baybo_store::AgentProfileStore>,
    paths: WorkspacePaths,
    _workspace: tempfile::TempDir,
}

async fn seed_agent(
    f: &Fixture,
    project: &ProjectId,
    handle: &str,
    framework: AgentFramework,
) -> AgentProfileId {
    let id = AgentProfileId::parse(handle).expect("agent id");
    let now = chrono::Utc::now();
    f.agents
        .create(&baybo_store::AgentProfileRow {
            id: id.clone(),
            description: String::new(),
            avatar_blob_id: None,
            framework,
            llm: None,
            builtin: false,
            team: Some(TeamMembership {
                project_id: project.clone(),
                handle: AgentHandle::parse(handle).expect("agent handle"),
            }),
            hired_by: None,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("seed agent");
    id
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
    let dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>> = Arc::default();
    Fixture {
        manager: ProjectManager::new(
            Arc::clone(&store.project),
            Arc::clone(&store.agent_profile),
            paths.clone(),
            Arc::new(baybo_project::NoopProjectEvents),
            {
                let seen = Arc::clone(&dispatched);
                Arc::new(move |run| seen.lock().push(run))
            },
        ),
        agents: Arc::clone(&store.agent_profile),
        store: Arc::clone(&store.project),
        dispatched,
        paths,
        _workspace: workspace,
    }
}

impl Fixture {
    async fn store_settle(&self, run: &baybo_model::IssueRunId, status: RunStatus) {
        self.store
            .settle_run(run, status, None)
            .await
            .expect("settle");
    }
}

fn new_project(name: &str) -> NewProject {
    NewProject {
        name: name.to_owned(),
        description: String::new(),
        workdir: None,
        daily_budget: None,
    }
}

fn new_issue(title: &str) -> NewIssueRequest {
    NewIssueRequest {
        title: title.to_owned(),
        description: String::new(),
        status: IssueStatus::Backlog,
        priority: IssuePriority::None,
        assignee: None,
        parent: None,
        stage: 0,
        source_key: None,
    }
}

#[tokio::test]
async fn an_empty_workdir_is_materialised_as_a_repo_under_work() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("My Board"))
        .await
        .expect("create");

    let expected = f.paths.work_dir().join("my-board");
    assert_eq!(
        std::path::Path::new(&project.workdir),
        expected,
        "the name became a directory name"
    );
    assert!(expected.join(".git").exists(), "and it is a git repository");
}

#[tokio::test]
async fn a_new_project_comes_with_a_lead() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Staffed"))
        .await
        .expect("create");

    let team = f.manager.team(&project.id).await.expect("team");
    assert_eq!(team.len(), 1, "exactly the lead");
    let lead = &team[0];
    let membership = lead.team.as_ref().expect("the lead is on the team");
    assert_eq!(membership.project_id, project.id);
    assert_eq!(membership.handle.as_str(), baybo_project::LEAD_HANDLE);
    assert!(
        lead.hired_by.is_none(),
        "nobody hired the lead; it comes with the board"
    );
    assert_eq!(lead.framework, AgentFramework::Baybo);

    let soul = tokio::fs::read_to_string(
        lead.id
            .identity_file(&f.paths, baybo_workspace::IdentityKind::Soul),
    )
    .await
    .expect("soul");
    assert!(
        soul.contains("You coordinate one project's board"),
        "{soul}"
    );
    let identity = tokio::fs::read_to_string(
        lead.id
            .identity_file(&f.paths, baybo_workspace::IdentityKind::Identity),
    )
    .await
    .expect("identity");
    assert_eq!(
        baybo_workspace::display_name(&identity).as_deref(),
        Some("Lead")
    );

    let global = f.agents.list().await.expect("global roster");
    assert!(
        global.iter().all(|row| row.id != lead.id),
        "the lead must not appear in the global agent list"
    );

    let other = f
        .manager
        .create_project(new_project("Also Staffed"))
        .await
        .expect("create");
    let other_lead = &f.manager.team(&other.id).await.expect("team")[0];
    assert_ne!(other_lead.id, lead.id);
    assert_eq!(
        other_lead
            .team
            .as_ref()
            .map(|t| t.handle.as_str().to_owned()),
        Some(baybo_project::LEAD_HANDLE.to_owned())
    );
}

#[tokio::test]
async fn the_lead_can_be_assigned_work() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Solo"))
        .await
        .expect("p");
    let lead = f.manager.team(&project.id).await.expect("team")[0]
        .id
        .clone();

    let issue = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(lead.clone()),
                ..new_issue("do it yourself")
            },
        )
        .await
        .expect("the lead can take work")
        .into_issue();
    assert_eq!(issue.assignee, Some(lead));
    assert_eq!(f.dispatched.lock().len(), 1, "and starting it runs");
}

fn new_member(name: &str) -> baybo_project::NewTeamMember {
    baybo_project::NewTeamMember {
        name: name.to_owned(),
        role: "Writes the tests nobody else wants to.".to_owned(),
        framework: None,
        llm: None,
    }
}

#[tokio::test]
async fn a_hire_gets_a_handle_a_soul_and_a_name() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Hiring"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();

    let hired = f
        .manager
        .hire(&p.id, new_member("Test Engineer"), Some(lead.clone()))
        .await
        .expect("hire");
    assert_eq!(
        hired.team.as_ref().map(|t| t.handle.as_str()),
        Some("test-engineer"),
        "the handle comes from the name"
    );
    assert_eq!(hired.hired_by, Some(lead), "and the hire names who made it");

    let soul = tokio::fs::read_to_string(
        hired
            .id
            .identity_file(&f.paths, baybo_workspace::IdentityKind::Soul),
    )
    .await
    .expect("soul");
    assert!(
        soul.contains("Writes the tests nobody else wants to."),
        "the role seeds the soul: {soul}"
    );
    assert!(
        !soul.contains("{{role}}"),
        "and the placeholder is substituted, not shipped: {soul}"
    );

    let team = f.manager.team(&p.id).await.expect("team");
    assert_eq!(team.len(), 2);
    assert!(
        f.agents
            .list()
            .await
            .expect("global")
            .iter()
            .all(|row| row.id != hired.id)
    );
}

#[tokio::test]
async fn a_taken_handle_is_numbered_rather_than_reused() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Numbering"))
        .await
        .expect("p");

    let first = f
        .manager
        .hire(&p.id, new_member("QA"), None)
        .await
        .expect("hire");
    assert_eq!(first.team.as_ref().map(|t| t.handle.as_str()), Some("qa"));

    let second = f
        .manager
        .hire(&p.id, new_member("QA"), None)
        .await
        .expect("hire");
    assert_eq!(
        second.team.as_ref().map(|t| t.handle.as_str()),
        Some("qa-2")
    );

    f.manager
        .remove_from_team(&p.id, &first.id)
        .await
        .expect("remove");
    let third = f
        .manager
        .hire(&p.id, new_member("QA"), None)
        .await
        .expect("hire");
    assert_eq!(third.team.as_ref().map(|t| t.handle.as_str()), Some("qa-3"));
}

#[tokio::test]
async fn hiring_refuses_a_nameless_agent_and_a_full_team() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Full"))
        .await
        .expect("p");

    for (member, why) in [
        (
            baybo_project::NewTeamMember {
                name: "   ".to_owned(),
                ..new_member("x")
            },
            "a blank name",
        ),
        (
            baybo_project::NewTeamMember {
                name: "!!!".to_owned(),
                ..new_member("x")
            },
            "a name with no handle in it",
        ),
        (
            baybo_project::NewTeamMember {
                role: "  ".to_owned(),
                ..new_member("Roleless")
            },
            "a missing role",
        ),
    ] {
        let refused = f.manager.hire(&p.id, member, None).await.expect_err(why);
        assert!(matches!(refused, ProjectError::Invalid { .. }), "{why}");
    }

    for n in 1..baybo_project::MAX_TEAM_AGENTS {
        f.manager
            .hire(&p.id, new_member(&format!("Dev {n}")), None)
            .await
            .unwrap_or_else(|e| panic!("hire {n}: {e}"));
    }
    let refused = f
        .manager
        .hire(&p.id, new_member("One Too Many"), None)
        .await
        .expect_err("the cap holds");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
    assert_eq!(
        f.manager.team(&p.id).await.expect("team").len(),
        baybo_project::MAX_TEAM_AGENTS
    );
}

#[tokio::test]
async fn the_lead_cannot_be_removed_and_neither_can_a_busy_agent() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Removal"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();

    let refused = f
        .manager
        .remove_from_team(&p.id, &lead)
        .await
        .expect_err("a board keeps its coordinator");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    let dev = f
        .manager
        .hire(&p.id, new_member("Dev"), None)
        .await
        .expect("hire");
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.id.clone()),
                ..new_issue("in flight")
            },
        )
        .await
        .expect("start work")
        .into_issue();

    let refused = f
        .manager
        .remove_from_team(&p.id, &dev.id)
        .await
        .expect_err("removing somebody mid-run only hides who is working");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    f.manager.cancel_run(&p.id, 1).await.expect("cancel");
    f.manager
        .remove_from_team(&p.id, &dev.id)
        .await
        .expect("remove");
    assert_eq!(f.manager.team(&p.id).await.expect("team").len(), 1);
    assert!(f.manager.remove_from_team(&p.id, &dev.id).await.is_err());
}

#[tokio::test]
async fn removal_is_scoped_to_the_board_that_asks() {
    let f = fixture().await;
    let mine = f
        .manager
        .create_project(new_project("mine"))
        .await
        .expect("p");
    let theirs = f
        .manager
        .create_project(new_project("theirs"))
        .await
        .expect("p");
    let outsider = f
        .manager
        .hire(&theirs.id, new_member("Dev"), None)
        .await
        .expect("hire");

    let refused = f
        .manager
        .remove_from_team(&mine.id, &outsider.id)
        .await
        .expect_err("not this board's agent");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
    assert_eq!(f.manager.team(&theirs.id).await.expect("team").len(), 2);
}

#[tokio::test]
async fn a_non_empty_work_directory_is_never_silently_adopted() {
    let f = fixture().await;
    let squatted = f.paths.work_dir().join("taken");
    tokio::fs::create_dir_all(&squatted).await.expect("mkdir");
    tokio::fs::write(squatted.join("notes.txt"), b"someone else's")
        .await
        .expect("write");

    let refused = f
        .manager
        .create_project(new_project("taken"))
        .await
        .expect_err("an occupied directory must not be adopted");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
    assert!(
        squatted.join("notes.txt").exists(),
        "and the refusal touched nothing"
    );
}

#[tokio::test]
async fn a_supplied_workdir_must_be_an_existing_repo() {
    let f = fixture().await;
    let outside = tempfile::tempdir().expect("tempdir");

    let not_a_repo = f
        .manager
        .create_project(NewProject {
            workdir: Some(outside.path().to_string_lossy().into_owned()),
            ..new_project("plain dir")
        })
        .await
        .expect_err("a directory that is not a repo cannot host worktrees");
    assert!(matches!(not_a_repo, ProjectError::Invalid { .. }));

    let relative = f
        .manager
        .create_project(NewProject {
            workdir: Some("relative/path".into()),
            ..new_project("relative")
        })
        .await
        .expect_err("a relative path resolves against nothing in particular");
    assert!(matches!(relative, ProjectError::Invalid { .. }));

    std::fs::create_dir_all(outside.path().join(".git")).expect("fake repo");
    let ok = f
        .manager
        .create_project(NewProject {
            workdir: Some(outside.path().to_string_lossy().into_owned()),
            ..new_project("real repo")
        })
        .await
        .expect("an existing repo is accepted");
    assert_eq!(
        std::path::Path::new(&ok.workdir),
        outside.path(),
        "and stored verbatim"
    );
}

#[tokio::test]
async fn a_workdir_inside_the_workspace_is_refused() {
    let f = fixture().await;
    let inside = f.paths.root().join("state");
    tokio::fs::create_dir_all(inside.join(".git"))
        .await
        .expect("mkdir");

    let refused = f
        .manager
        .create_project(NewProject {
            workdir: Some(inside.to_string_lossy().into_owned()),
            ..new_project("inside")
        })
        .await
        .expect_err("the workspace's own tree is not a checkout");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_workdir_that_swallows_the_workspace_is_refused() {
    let f = fixture().await;
    let parent = f
        .paths
        .root()
        .parent()
        .expect("workspace has a parent")
        .to_path_buf();
    tokio::fs::create_dir_all(parent.join(".git"))
        .await
        .expect("mkdir");

    let refused = f
        .manager
        .create_project(NewProject {
            workdir: Some(parent.to_string_lossy().into_owned()),
            ..new_project("swallows")
        })
        .await
        .expect_err("a parent of the workspace contains the workspace");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_workdir_symlinked_into_the_workspace_is_refused_too() {
    let f = fixture().await;
    let secret = f.paths.root().join("state");
    tokio::fs::create_dir_all(secret.join(".git"))
        .await
        .expect("mkdir");
    let outside = tempfile::tempdir().expect("tempdir");
    let link = outside.path().join("innocent-looking-repo");
    std::os::unix::fs::symlink(&secret, &link).expect("symlink");

    let refused = f
        .manager
        .create_project(NewProject {
            workdir: Some(link.to_string_lossy().into_owned()),
            ..new_project("symlinked")
        })
        .await
        .expect_err("a symlink into the workspace is still the workspace");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn blank_and_oversized_names_are_refused() {
    let f = fixture().await;
    for bad in ["", "   ", "\t\n"] {
        let refused = f
            .manager
            .create_project(new_project(bad))
            .await
            .expect_err("a blank name is not a name");
        assert!(matches!(refused, ProjectError::Invalid { .. }));
    }
    let refused = f
        .manager
        .create_project(new_project(&"x".repeat(65)))
        .await
        .expect_err("names have a ceiling");
    assert!(matches!(refused, ProjectError::Invalid { .. }));
}

#[tokio::test]
async fn an_archived_project_is_read_only() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("archivable"))
        .await
        .expect("create");
    f.manager
        .create_issue(&project.id, IssueActor::User, new_issue("before"))
        .await
        .expect("create issue")
        .into_issue();
    f.manager
        .set_project_archived(&project.id, true)
        .await
        .expect("archive");

    let refused = f
        .manager
        .create_issue(&project.id, IssueActor::User, new_issue("after"))
        .await
        .expect_err("an archived board takes no new work");
    assert!(matches!(refused, ProjectError::Archived(_)), "{refused:?}");

    let refused = f
        .manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: "renamed".into(),
                description: String::new(),
                daily_budget: None,
            },
        )
        .await
        .expect_err("nor a rename");
    assert!(matches!(refused, ProjectError::Archived(_)));

    let issues = f.manager.list_issues(&project.id).await.expect("list");
    assert_eq!(issues.len(), 1);

    f.manager
        .set_project_archived(&project.id, false)
        .await
        .expect("restore");
    f.manager
        .create_issue(&project.id, IssueActor::User, new_issue("after restore"))
        .await
        .expect("writable again")
        .into_issue();
}

#[tokio::test]
async fn issues_answer_only_within_their_own_project() {
    let f = fixture().await;
    let a = f.manager.create_project(new_project("a")).await.expect("a");
    let b = f.manager.create_project(new_project("b")).await.expect("b");
    f.manager
        .create_issue(&a.id, IssueActor::User, new_issue("a's first"))
        .await
        .expect("issue")
        .into_issue();

    let refused = f
        .manager
        .get_issue(&b.id, 1)
        .await
        .expect_err("b has no #1 yet");
    assert!(matches!(refused, ProjectError::NoSuchIssue { .. }));

    let unknown = baybo_model::ProjectId::generate();
    let refused = f
        .manager
        .list_issues(&unknown)
        .await
        .expect_err("an unknown board is not an empty board");
    assert!(matches!(refused, ProjectError::NoSuchProject(_)));
}

#[tokio::test]
async fn a_patch_that_sets_nothing_is_refused() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    f.manager
        .create_issue(&p.id, IssueActor::User, new_issue("something"))
        .await
        .expect("issue")
        .into_issue();

    let refused = f
        .manager
        .update_issue(&p.id, 1, IssueActor::User, IssueUpdate::default())
        .await
        .expect_err("an empty patch is a caller mistake, not a no-op write");
    assert!(matches!(refused, ProjectError::Invalid { .. }));

    let issue = f
        .manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(Some("   ".into())),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    assert!(issue.blocked_reason.is_none());
}

#[tokio::test]
async fn a_move_must_name_the_issue_it_moves() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    for title in ["one", "two"] {
        f.manager
            .create_issue(&p.id, IssueActor::User, new_issue(title))
            .await
            .expect("issue")
            .into_issue();
    }

    let refused = f
        .manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[2])
        .await
        .expect_err("the moved issue must appear in its destination");
    assert!(matches!(refused, ProjectError::Invalid { .. }));

    let moved = f
        .manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("move");
    assert_eq!(moved.status, IssueStatus::Todo);
    assert_eq!(moved.position, 0);
}

#[tokio::test]
async fn in_progress_needs_somebody_on_it() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(&p.id, IssueActor::User, new_issue("unclaimed"))
        .await
        .expect("issue")
        .into_issue();

    let refused = f
        .manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::InProgress, &[1])
        .await
        .expect_err("an unassigned card cannot start");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("todo takes unassigned work");

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(dev.clone())),
                ..Default::default()
            },
        )
        .await
        .expect("assign");
    let moved = f
        .manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::InProgress, &[1])
        .await
        .expect("assigned work starts");
    assert_eq!(moved.assignee.as_ref(), Some(&dev));

    let refused = f
        .manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect_err("unassigning in-flight work recreates the zombie");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn an_assignee_must_exist_and_must_be_able_to_run() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let external = seed_agent(&f, &p.id, "codex-1", AgentFramework::Codex).await;

    let ghost = AgentProfileId::parse("nobody").expect("id");
    let refused = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(ghost),
                ..new_issue("to a ghost")
            },
        )
        .await
        .expect_err("an agent that does not exist cannot be assigned");
    assert!(matches!(refused, ProjectError::Invalid { .. }));

    let refused = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(external),
                ..new_issue("to codex")
            },
        )
        .await
        .expect_err("external frameworks cannot host an issue session");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn an_assignee_must_be_on_this_board() {
    let f = fixture().await;
    let mine = f
        .manager
        .create_project(new_project("mine"))
        .await
        .expect("p");
    let theirs = f
        .manager
        .create_project(new_project("theirs"))
        .await
        .expect("p");

    let global = AgentProfileId::parse("global-1").expect("id");
    let now = chrono::Utc::now();
    f.agents
        .create(&baybo_store::AgentProfileRow {
            id: global.clone(),
            description: String::new(),
            avatar_blob_id: None,
            framework: AgentFramework::Baybo,
            llm: None,
            builtin: false,
            team: None,
            hired_by: None,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("seed global agent");
    let outsider = seed_agent(&f, &theirs.id, "dev-1", AgentFramework::Baybo).await;

    for (assignee, why) in [
        (global, "a global agent is on nobody's team"),
        (outsider, "another board's teammate is not on this one"),
    ] {
        let refused = f
            .manager
            .create_issue(
                &mine.id,
                IssueActor::User,
                NewIssueRequest {
                    assignee: Some(assignee),
                    ..new_issue("to an outsider")
                },
            )
            .await
            .expect_err(why);
        assert!(matches!(refused, ProjectError::Invalid { .. }), "{why}");
    }
}

#[tokio::test]
async fn a_removed_teammate_takes_no_new_work() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    assert!(f.agents.remove_from_team(&dev).await.expect("remove"));

    let refused = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                ..new_issue("to somebody who left")
            },
        )
        .await
        .expect_err("a removed teammate cannot be assigned");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
    assert!(
        f.agents.get(&dev).await.expect("get").is_some(),
        "the row survives so past work can still name it"
    );
}

#[tokio::test]
async fn a_card_reaching_in_progress_records_a_run_before_anything_starts() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                ..new_issue("do the thing")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    assert!(
        f.dispatched.lock().is_empty(),
        "an issue created in the backlog starts nothing"
    );

    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::InProgress, &[1])
        .await
        .expect("start");

    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Queued);
    assert_eq!(runs[0].trigger, RunTrigger::Started);
    assert_eq!(runs[0].agent_id, dev);

    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1, "and exactly one run was handed out");
    assert_eq!(announced[0].id, runs[0].id);

    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Review, &[1])
        .await
        .expect("move out");
    assert_eq!(f.dispatched.lock().len(), 1);
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].status,
        RunStatus::Queued,
        "the run outlives the column it was started from"
    );
}

#[tokio::test]
async fn assigning_work_already_in_flight_starts_it_and_never_twice() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let other = seed_agent(&f, &p.id, "dev-2", AgentFramework::Baybo).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("starts immediately")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "creation into the column starts work"
    );

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(other)),
                ..Default::default()
            },
        )
        .await
        .expect("reassign");
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "an issue holds one run at a time"
    );
    assert_eq!(f.manager.list_runs(&p.id, 1).await.expect("runs").len(), 1);
}

#[tokio::test]
async fn reassigning_live_work_starts_the_new_agent() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let other = seed_agent(&f, &p.id, "dev-2", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("dev-1 tried and failed")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    let first = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store_settle(&first, RunStatus::Failed).await;
    f.dispatched.lock().clear();

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(other.clone())),
                ..Default::default()
            },
        )
        .await
        .expect("hand it to dev-2");

    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1, "the new assignee was started");
    assert_eq!(announced[0].agent_id, other);
    assert_eq!(announced[0].trigger, RunTrigger::Assigned);
    assert_eq!(f.manager.list_runs(&p.id, 1).await.expect("runs").len(), 2);
}

#[tokio::test]
async fn a_cancelled_card_never_starts_a_run() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let other = seed_agent(&f, &p.id, "dev-2", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("called off")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    let first = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store_settle(&first, RunStatus::Cancelled).await;
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("cancel the card");
    f.dispatched.lock().clear();

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(other)),
                ..Default::default()
            },
        )
        .await
        .expect("reassign the cancelled card");
    assert!(
        f.dispatched.lock().is_empty(),
        "reassigning abandoned work must not start an agent on it"
    );
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        1,
        "and no ledger row is written either"
    );

    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("out of the column");
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::InProgress, &[1])
        .await
        .expect("and back in");
    assert!(
        f.dispatched.lock().is_empty(),
        "a cancelled card entering In Progress starts nothing either"
    );

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("revive it");
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(dev)),
                ..Default::default()
            },
        )
        .await
        .expect("hand it back to somebody");
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "a revived card takes work again"
    );
}

#[tokio::test]
async fn retry_is_refused_on_a_card_the_board_has_finished_with() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("stopped")
            },
        )
        .await
        .expect("issue");
    let run = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store_settle(&run, RunStatus::Failed).await;
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("cancel the card");
    f.dispatched.lock().clear();

    let refused = f
        .manager
        .retry_run(&p.id, 1)
        .await
        .expect_err("a cancelled card cannot be retried");
    assert!(
        matches!(&refused, ProjectError::Invalid { field, reason }
            if *field == "issue"
                && reason == "this issue was cancelled — reopen it before running it again"),
        "{refused:?}"
    );
    assert!(f.dispatched.lock().is_empty());
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        1,
        "and nothing was recorded either"
    );
}

#[tokio::test]
async fn the_retry_refusals_say_exactly_what_the_button_predicts() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let refusal = async |number: i64| match f.manager.retry_run(&p.id, number).await {
        Err(ProjectError::Invalid { reason, .. }) => reason,
        other => panic!("#{number} should have been refused: {other:?}"),
    };

    f.manager
        .create_issue(&p.id, IssueActor::User, new_issue("nobody on it"))
        .await
        .expect("issue");
    assert_eq!(refusal(1).await, "an issue with nobody on it cannot be run");

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                ..new_issue("called off")
            },
        )
        .await
        .expect("issue");
    f.manager
        .update_issue(
            &p.id,
            2,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("cancel");
    assert_eq!(
        refusal(2).await,
        "this issue was cancelled — reopen it before running it again"
    );

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Done,
                assignee: Some(dev),
                ..new_issue("finished")
            },
        )
        .await
        .expect("issue");
    assert_eq!(
        refusal(3).await,
        "this issue is done — move it back into the board before running it again"
    );

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("cancel");
    assert_eq!(refusal(1).await, "an issue with nobody on it cannot be run");
}

#[tokio::test]
async fn an_agent_that_moved_off_baybo_stops_getting_runs() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("assigned while it was still baybo")
            },
        )
        .await
        .expect("issue");
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        1,
        "the assign started one, which is the state the flip has to survive"
    );
    f.manager
        .cancel_run(&p.id, 1)
        .await
        .expect("free the card's slot");

    f.agents
        .update(
            &dev,
            &baybo_store::AgentProfileUpdate {
                description: String::new(),
                framework: AgentFramework::Codex,
            },
        )
        .await
        .expect("the operator moves dev-1 to codex");
    f.dispatched.lock().clear();

    match f.manager.retry_run(&p.id, 1).await {
        Err(ProjectError::Invalid { reason, .. }) => assert_eq!(
            reason,
            "dev-1 runs on codex, which cannot yet host an issue's session — assign a baybo agent"
        ),
        other => panic!("a codex assignee should have been refused: {other:?}"),
    }
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        1,
        "and no second row was recorded"
    );

    f.manager
        .comment(&p.id, 1, IssueActor::User, "@dev-1 pick this back up")
        .await
        .expect("comment");
    assert!(
        f.dispatched.lock().is_empty(),
        "a wake must not start a run the executor would only fail"
    );
}

#[tokio::test]
async fn a_retry_on_a_held_run_blames_the_budget_when_there_is_still_no_room() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Skint")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("work nobody can afford yet")
            },
        )
        .await
        .expect("issue");
    let held = f.manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
    assert_eq!(held.status, RunStatus::Held, "the board is broke");
    f.dispatched.lock().clear();

    match f.manager.retry_run(&p.id, 1).await {
        Err(ProjectError::Conflict(reason)) => assert_eq!(
            reason,
            "this run is held — the project is over its daily budget, and starts as soon as there is room"
        ),
        other => panic!("a broke board should have refused: {other:?}"),
    }
    assert!(
        f.dispatched.lock().is_empty(),
        "and nothing went out on a board with no room"
    );
    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    assert_eq!(runs.len(), 1, "the refused press recorded no second row");
    assert_eq!(runs[0].id, held.id, "and left the held one where it was");
    assert_eq!(runs[0].status, RunStatus::Held);
}

#[tokio::test]
async fn a_crash_leaves_runs_the_boot_sweep_hands_back() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("interrupted")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    f.dispatched.lock().clear();

    let resumed = f.manager.resume_unsettled_runs().await.expect("boot sweep");
    assert_eq!(resumed, 1);
    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1, "and hands it back out to be executed");
    assert_eq!(announced[0].status, RunStatus::Queued);
}

#[tokio::test]
async fn the_boot_sweep_calls_off_a_run_whose_card_was_cancelled() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let other = seed_agent(&f, &p.id, "dev-2", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("interrupted")
            },
        )
        .await
        .expect("issue");
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("cancel it while the process is down");
    f.dispatched.lock().clear();

    assert_eq!(
        f.manager.resume_unsettled_runs().await.expect("boot sweep"),
        0,
        "the count is what was re-driven, not what the sweep found"
    );
    assert!(f.dispatched.lock().is_empty());

    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Cancelled);
    assert!(runs[0].status.is_settled());
    let settled = f
        .manager
        .timeline(&p.id, 1)
        .await
        .expect("timeline")
        .into_iter()
        .find(|e| {
            matches!(
                e.body,
                baybo_store::project::IssueEventBody::RunSettled {
                    status: RunStatus::Cancelled,
                    ..
                }
            )
        })
        .expect("the card says the run was called off");
    assert_eq!(settled.actor, IssueActor::System);

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("revive");
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(other)),
                ..Default::default()
            },
        )
        .await
        .expect("staff it again");
    assert_eq!(f.dispatched.lock().len(), 1);
}

#[tokio::test]
async fn a_run_called_off_after_it_started_is_not_told_it_never_did() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    for title in ["claimed", "never claimed"] {
        f.manager
            .create_issue(
                &p.id,
                IssueActor::User,
                NewIssueRequest {
                    status: IssueStatus::InProgress,
                    assignee: Some(dev.clone()),
                    ..new_issue(title)
                },
            )
            .await
            .expect("issue");
    }

    let session = baybo_model::SessionId::from("issue-1-dev-1");
    let claimed = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    assert!(
        f.store
            .claim_run(&claimed, &session)
            .await
            .expect("claim the run"),
        "the executor took it"
    );

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("cancel #1");
    f.manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Done, &[2])
        .await
        .expect("finish #2");
    f.dispatched.lock().clear();

    assert_eq!(
        f.manager.resume_unsettled_runs().await.expect("boot sweep"),
        0,
        "neither card takes work any more"
    );

    let started = f.manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
    let unstarted = f.manager.list_runs(&p.id, 2).await.expect("runs").remove(0);
    assert_eq!(started.status, RunStatus::Cancelled);
    assert_eq!(unstarted.status, RunStatus::Cancelled);
    assert_eq!(
        started.session_id,
        Some(session),
        "the settled row still opens the transcript of the work that was done"
    );

    let started_reason = started.error.expect("the card says why");
    let unstarted_reason = unstarted.error.expect("the card says why");
    assert!(
        !started_reason.contains("before this run started"),
        "the card already says this run started: {started_reason}"
    );
    assert!(
        started_reason.contains("interrupted"),
        "and says what actually happened to it: {started_reason}"
    );
    assert!(
        unstarted_reason.contains("before this run started"),
        "a row nothing ever claimed never did start: {unstarted_reason}"
    );

    let told = f
        .manager
        .timeline(&p.id, 1)
        .await
        .expect("timeline")
        .into_iter()
        .find_map(|e| match e.body {
            baybo_store::project::IssueEventBody::RunSettled { error, .. } => error,
            _ => None,
        })
        .expect("the card says the run was called off");
    assert_eq!(told, started_reason);
}

#[tokio::test]
async fn the_boot_sweep_leaves_an_archived_boards_runs_where_they_are() {
    let f = fixture().await;
    let shelved = f
        .manager
        .create_project(new_project("Shelved"))
        .await
        .expect("archived board");
    let live = f
        .manager
        .create_project(new_project("Live"))
        .await
        .expect("live board");
    for (project, handle) in [(&shelved.id, "dev-1"), (&live.id, "dev-2")] {
        let dev = seed_agent(&f, project, handle, AgentFramework::Baybo).await;
        f.manager
            .create_issue(
                project,
                IssueActor::User,
                NewIssueRequest {
                    status: IssueStatus::InProgress,
                    assignee: Some(dev),
                    ..new_issue("interrupted")
                },
            )
            .await
            .expect("issue");
    }
    let shelved_run = f.manager.list_runs(&shelved.id, 1).await.expect("runs")[0]
        .id
        .clone();
    assert!(
        f.store
            .claim_run(&shelved_run, &baybo_model::SessionId::from("shelved-1"))
            .await
            .expect("claim"),
        "an executor had it when the board was put away"
    );
    f.manager
        .set_project_archived(&shelved.id, true)
        .await
        .expect("archive");
    f.dispatched.lock().clear();

    assert_eq!(
        f.manager.resume_unsettled_runs().await.expect("boot sweep"),
        1,
        "only the live board's run is re-driven"
    );
    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].project_id, live.id);

    let shelved_runs = f.manager.list_runs(&shelved.id, 1).await.expect("runs");
    assert_eq!(shelved_runs.len(), 1, "and none was recorded in its place");
    assert!(
        !shelved_runs[0].status.is_settled(),
        "the work is shelved, not called off: {:?}",
        shelved_runs[0].status
    );
    assert!(
        !f.manager
            .timeline(&shelved.id, 1)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(
                e.body,
                baybo_store::project::IssueEventBody::RunSettled { .. }
            )),
        "an archived board's card was told its run was called off"
    );
}

#[tokio::test]
async fn restoring_a_board_hands_its_shelved_run_back_out() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Shelved"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("interrupted")
            },
        )
        .await
        .expect("issue");
    let recorded = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.manager
        .set_project_archived(&p.id, true)
        .await
        .expect("archive");
    f.manager.resume_unsettled_runs().await.expect("boot sweep");
    f.dispatched.lock().clear();

    f.manager
        .set_project_archived(&p.id, false)
        .await
        .expect("restore");

    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1, "the shelved run is handed back out");
    assert_eq!(
        announced[0].id, recorded,
        "the same run resumes rather than a second being recorded for the same work"
    );
    assert_eq!(announced[0].status, RunStatus::Queued);
    assert_eq!(f.manager.list_runs(&p.id, 1).await.expect("runs").len(), 1);
}

#[tokio::test]
async fn a_board_that_was_never_archived_is_not_re_dispatched_by_a_restore() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Live"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("already under way")
            },
        )
        .await
        .expect("issue");
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "the card's own run went out when it started"
    );
    f.dispatched.lock().clear();

    let restored = f
        .manager
        .set_project_archived(&p.id, false)
        .await
        .expect("restoring a live board is not an error");
    assert!(restored.archived_at.is_none());
    assert!(
        f.dispatched.lock().is_empty(),
        "a board that was never away owes nothing, and its live run is somebody else's"
    );

    f.manager
        .set_project_archived(&p.id, true)
        .await
        .expect("archive");
    let again = f
        .manager
        .set_project_archived(&p.id, true)
        .await
        .expect("archiving a board that is already away is not an error either");
    assert!(again.archived_at.is_some());

    let unknown = baybo_model::ProjectId::generate();
    let refused = f
        .manager
        .set_project_archived(&unknown, true)
        .await
        .expect_err("but a board that does not exist still is");
    assert!(matches!(refused, ProjectError::NoSuchProject(_)));
}

struct HeldRunsUnreadable(Arc<dyn baybo_store::project::ProjectStore>);

macro_rules! forwards_everything_else {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        #[async_trait::async_trait]
        impl baybo_store::project::ProjectStore for HeldRunsUnreadable {
            async fn held_runs(&self, _project: &ProjectId) -> StoreResult<Vec<IssueRunRow>> {
                Err(baybo_store::StorageError::Storage(
                    "this board's holds are unreadable".into(),
                ))
            }
            $(async fn $name(&self, $($arg: $ty),*) -> $ret {
                self.0.$name($($arg),*).await
            })*
        }
    };
}

forwards_everything_else! {
    list_projects(include_archived: bool) -> StoreResult<Vec<ProjectRow>>;
    get_project(id: &ProjectId) -> StoreResult<Option<ProjectRow>>;
    create_project(row: &ProjectRow) -> StoreResult<()>;
    update_project(id: &ProjectId, update: &ProjectUpdate) -> StoreResult<bool>;
    spend_since(project: &ProjectId, since: DateTime<Utc>) -> StoreResult<baybo_model::MicroUsd>;
    attention() -> StoreResult<Vec<(ProjectId, AttentionCounts)>>;
    projects_for_sessions(sessions: &[SessionId]) -> StoreResult<Vec<(SessionId, ProjectId)>>;
    project_feed(project: &ProjectId, before: Option<DateTime<Utc>>, limit: usize)
        -> StoreResult<Vec<IssueEventRow>>;
    live_issue_by_source_key(project: &ProjectId, source_key: &str) -> StoreResult<Option<IssueRow>>;
    list_children(parent: &IssueId) -> StoreResult<Vec<IssueRow>>;
    hold_run(id: &IssueRunId) -> StoreResult<bool>;
    release_run(id: &IssueRunId) -> StoreResult<bool>;
    mark_project_read(id: &ProjectId, at: DateTime<Utc>) -> StoreResult<bool>;
    set_project_archived(id: &ProjectId, archived: bool) -> StoreResult<bool>;
    list_issues(project: &ProjectId) -> StoreResult<Vec<IssueRow>>;
    get_issue(project: &ProjectId, number: i64) -> StoreResult<Option<IssueRow>>;
    create_issue(new: &NewIssue) -> StoreResult<IssueRow>;
    update_issue(project: &ProjectId, number: i64, update: &IssueUpdate) -> StoreResult<bool>;
    move_issue(project: &ProjectId, number: i64, status: IssueStatus, ordered_numbers: &[i64])
        -> StoreResult<bool>;
    enqueue_run(new: &NewIssueRun) -> StoreResult<IssueRunRow>;
    append_event(new: &NewIssueEvent) -> StoreResult<IssueEventRow>;
    list_events(issue: &IssueId) -> StoreResult<Vec<IssueEventRow>>;
    events_since(issue: &IssueId, since: DateTime<Utc>) -> StoreResult<Vec<IssueEventRow>>;
    set_issue_branch(id: &IssueId, branch: &str) -> StoreResult<bool>;
    list_runs(issue: &IssueId) -> StoreResult<Vec<IssueRunRow>>;
    active_runs(project: &ProjectId) -> StoreResult<Vec<IssueRunRow>>;
    get_run(id: &IssueRunId) -> StoreResult<Option<IssueRunRow>>;
    claim_run(id: &IssueRunId, session: &SessionId) -> StoreResult<bool>;
    settle_run(id: &IssueRunId, status: RunStatus, error: Option<&str>) -> StoreResult<bool>;
    requeue_unsettled() -> StoreResult<()>;
}

#[tokio::test]
async fn a_board_that_cannot_read_its_holds_still_reports_what_it_re_drove() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Half broken"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("interrupted")
            },
        )
        .await
        .expect("issue");

    let dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>> = Arc::default();
    let manager = ProjectManager::new(
        Arc::new(HeldRunsUnreadable(Arc::clone(&f.store))),
        Arc::clone(&f.agents),
        f.paths.clone(),
        Arc::new(baybo_project::NoopProjectEvents),
        {
            let seen = Arc::clone(&dispatched);
            Arc::new(move |run| seen.lock().push(run))
        },
    );

    assert_eq!(
        manager
            .resume_unsettled_runs()
            .await
            .expect("the sweep still answers for the boards it could sweep"),
        1,
        "the run that went out is still counted"
    );
    assert_eq!(
        dispatched.lock().len(),
        1,
        "and it really did go out — an agent is working"
    );
}

#[tokio::test]
async fn the_board_writes_its_own_history() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Trail"))
        .await
        .expect("create");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;

    let issue = f
        .manager
        .create_issue(&project.id, IssueActor::User, new_issue("Wire it"))
        .await
        .expect("create issue")
        .into_issue();
    f.manager
        .update_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(dev.clone())),
                ..Default::default()
            },
        )
        .await
        .expect("assign");
    f.manager
        .move_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueStatus::InProgress,
            &[issue.number],
        )
        .await
        .expect("move");
    f.manager
        .comment(
            &project.id,
            issue.number,
            IssueActor::User,
            "  check the reconnect path  ",
        )
        .await
        .expect("comment");

    let timeline = f
        .manager
        .timeline(&project.id, issue.number)
        .await
        .expect("timeline");
    assert_eq!(
        timeline.iter().map(|e| e.body.kind()).collect::<Vec<_>>(),
        vec!["opened", "assigned", "moved", "comment"],
        "oldest first, and every board action left a mark: {timeline:?}"
    );
    assert!(
        matches!(
            &timeline[3].body,
            baybo_store::project::IssueEventBody::Comment { text } if text == "check the reconnect path"
        ),
        "a comment is stored trimmed: {:?}",
        timeline[3].body
    );
}

#[tokio::test]
async fn an_empty_comment_is_refused_rather_than_recorded() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Quiet"))
        .await
        .expect("create");
    let issue = f
        .manager
        .create_issue(&project.id, IssueActor::User, new_issue("Nothing to say"))
        .await
        .expect("create issue")
        .into_issue();

    let err = f
        .manager
        .comment(&project.id, issue.number, IssueActor::User, "   \n  ")
        .await
        .expect_err("whitespace is not a comment");
    assert!(matches!(err, ProjectError::Invalid { .. }), "{err:?}");
    assert_eq!(
        f.manager
            .timeline(&project.id, issue.number)
            .await
            .expect("timeline")
            .len(),
        1,
        "only the opening entry — the refusal wrote nothing"
    );
}

#[tokio::test]
async fn a_comment_on_live_work_starts_a_run_and_a_comment_on_parked_work_does_not() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Wake"))
        .await
        .expect("create");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;

    let parked = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                ..new_issue("Later")
            },
        )
        .await
        .expect("create parked")
        .into_issue();
    f.manager
        .comment(&project.id, parked.number, IssueActor::User, "some day")
        .await
        .expect("comment");
    assert!(
        f.dispatched.lock().is_empty(),
        "a comment in Backlog wakes nobody"
    );

    let live = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Todo,
                assignee: Some(dev.clone()),
                ..new_issue("Now")
            },
        )
        .await
        .expect("create live")
        .into_issue();
    f.manager
        .comment(&project.id, live.number, IssueActor::User, "have a look")
        .await
        .expect("comment");

    let runs = f.dispatched.lock().clone();
    assert_eq!(runs.len(), 1, "exactly one run, on the live card");
    assert_eq!(runs[0].number, live.number);
    assert_eq!(runs[0].trigger, RunTrigger::Comment);
    assert_eq!(runs[0].agent_id, dev);
}

#[tokio::test]
async fn a_comment_while_a_run_is_queued_does_not_start_a_second() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Coalesce"))
        .await
        .expect("create");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    let issue = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("Working")
            },
        )
        .await
        .expect("create")
        .into_issue();
    assert_eq!(f.dispatched.lock().len(), 1);

    for text in ["also this", "and this"] {
        f.manager
            .comment(&project.id, issue.number, IssueActor::User, text)
            .await
            .expect("comment");
    }
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "two comments on a queued run are still one run"
    );
    assert_eq!(
        f.manager
            .comment_delivery(&project.id, issue.number)
            .await
            .expect("delivery"),
        baybo_project::CommentDelivery::WaitsForQueuedRun,
        "and the composer is told exactly that"
    );
}

#[tokio::test]
async fn finishing_an_issue_gives_its_worktree_back_and_says_so() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Reclaim"))
        .await
        .expect("create");
    let issue = f
        .manager
        .create_issue(&project.id, IssueActor::User, new_issue("Do it"))
        .await
        .expect("create issue")
        .into_issue();

    let root = baybo_project::worktree::worktree_root(&f.paths, &project.id, issue.number);
    let branch = baybo_project::worktree::branch_name(issue.number, &issue.title);
    baybo_project::worktree::ensure(std::path::Path::new(&project.workdir), &root, &branch)
        .await
        .expect("cut a worktree the way a run would");
    assert!(root.exists());

    f.manager
        .move_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueStatus::Done,
            &[issue.number],
        )
        .await
        .expect("finish it");

    assert!(!root.exists(), "the checkout is gone");
    let kinds: Vec<_> = f
        .manager
        .timeline(&project.id, issue.number)
        .await
        .expect("timeline")
        .iter()
        .map(|e| e.body.kind())
        .collect();
    assert!(
        kinds.contains(&"worktree_reclaimed"),
        "and the timeline says so: {kinds:?}"
    );
}

#[tokio::test]
async fn a_worktree_holding_uncommitted_work_survives_being_finished() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Careful"))
        .await
        .expect("create");
    let issue = f
        .manager
        .create_issue(&project.id, IssueActor::User, new_issue("Half done"))
        .await
        .expect("create issue")
        .into_issue();
    let root = baybo_project::worktree::worktree_root(&f.paths, &project.id, issue.number);
    let branch = baybo_project::worktree::branch_name(issue.number, &issue.title);
    baybo_project::worktree::ensure(std::path::Path::new(&project.workdir), &root, &branch)
        .await
        .expect("cut");
    tokio::fs::write(root.join("scratch.txt"), b"not committed")
        .await
        .expect("leave work behind");

    f.manager
        .update_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("cancel it");

    assert!(root.join("scratch.txt").exists(), "the work is still there");
    let kinds: Vec<_> = f
        .manager
        .timeline(&project.id, issue.number)
        .await
        .expect("timeline")
        .iter()
        .map(|e| e.body.kind())
        .collect();
    assert!(
        kinds.contains(&"worktree_kept"),
        "and the operator is told why rather than left to notice: {kinds:?}"
    );
}

#[tokio::test]
async fn a_board_over_budget_records_the_work_it_is_not_doing() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Skint")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;

    f.manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("work nobody can afford")
            },
        )
        .await
        .expect("create")
        .into_issue();

    assert!(
        f.dispatched.lock().is_empty(),
        "nothing was started against an exhausted budget"
    );
    let runs = f.manager.list_runs(&project.id, 1).await.expect("runs");
    assert_eq!(runs.len(), 1, "but the run was recorded");
    assert_eq!(runs[0].status, RunStatus::Held);
    assert!(
        !runs[0].status.is_settled(),
        "a held run holds the issue's dedupe slot"
    );

    let timeline = f.manager.timeline(&project.id, 1).await.expect("timeline");
    let held = timeline
        .iter()
        .find(|e| {
            matches!(
                e.body,
                baybo_store::project::IssueEventBody::BudgetExhausted { .. }
            )
        })
        .expect("the timeline says the run was held");
    assert!(
        matches!(
            held.body,
            baybo_store::project::IssueEventBody::BudgetExhausted {
                spent_micros: 0,
                limit_micros: 0
            }
        ),
        "{:?}",
        held.body
    );
    assert_eq!(held.actor, IssueActor::System);
}

#[tokio::test]
async fn a_raised_budget_releases_what_it_was_holding() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Thawing")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("waiting on money")
            },
        )
        .await
        .expect("create")
        .into_issue();
    assert!(f.dispatched.lock().is_empty());

    f.manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(5_000_000)),
            },
        )
        .await
        .expect("raise the ceiling");

    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "the held run started once there was room"
    );
    let runs = f.manager.list_runs(&project.id, 1).await.expect("runs");
    assert_eq!(runs[0].status, RunStatus::Queued);
    assert!(
        f.manager
            .timeline(&project.id, 1)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(
                e.body,
                baybo_store::project::IssueEventBody::BudgetRestored { .. }
            ) && e.actor == IssueActor::System),
        "and the card says the board released it — the operator raised the \
         ceiling, they did not start this run"
    );
}

#[tokio::test]
async fn the_next_enqueue_releases_a_hold_the_budget_no_longer_justifies() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Rollover")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("held on Monday")
            },
        )
        .await
        .expect("create")
        .into_issue();
    assert!(f.dispatched.lock().is_empty(), "nothing to spend it on");

    f.store
        .update_project(
            &project.id,
            &ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(5_000_000)),
            },
        )
        .await
        .expect("a new day's ceiling");

    f.manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("dragged on Tuesday")
            },
        )
        .await
        .expect("create")
        .into_issue();

    let announced = f.dispatched.lock().clone();
    assert_eq!(
        announced.len(),
        2,
        "Tuesday's drag started Monday's hold as well as itself"
    );
    assert_eq!(announced[0].number, 1, "the older hold went first");
    assert_eq!(announced[1].number, 2);
    assert_eq!(
        f.manager.list_runs(&project.id, 1).await.expect("runs")[0].status,
        RunStatus::Queued,
        "and #1 no longer sits on its own dedupe slot"
    );
    assert!(
        f.manager
            .timeline(&project.id, 1)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(
                e.body,
                baybo_store::project::IssueEventBody::BudgetRestored { .. }
            )),
        "and #1's card says so"
    );
}

#[tokio::test]
async fn touching_the_held_card_itself_releases_it() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Stuck")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("the only card on the board")
            },
        )
        .await
        .expect("create")
        .into_issue();
    f.store
        .update_project(
            &project.id,
            &ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(5_000_000)),
            },
        )
        .await
        .expect("a new day's ceiling");
    f.dispatched.lock().clear();

    let started = f
        .manager
        .retry_run(&project.id, 1)
        .await
        .expect("the press is what releases it");
    assert_eq!(started.status, RunStatus::Queued);
    assert_eq!(f.dispatched.lock().len(), 1, "and it did start");
    let runs = f.manager.list_runs(&project.id, 1).await.expect("runs");
    assert_eq!(runs.len(), 1, "one row, released — not a second beside it");
    assert_eq!(runs[0].id, started.id);
}

#[tokio::test]
async fn a_negative_budget_is_refused() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Negative"))
        .await
        .expect("p");
    let refused = f
        .manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(-1)),
            },
        )
        .await
        .expect_err("a negative ceiling means nothing");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_released_hold_never_lands_on_a_card_the_board_has_finished_with() {
    for called_off in [true, false] {
        let f = fixture().await;
        let project = f
            .manager
            .create_project(NewProject {
                daily_budget: Some(baybo_model::MicroUsd::ZERO),
                ..new_project("Skint")
            })
            .await
            .expect("p");
        let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
        f.manager
            .create_issue(
                &project.id,
                IssueActor::User,
                NewIssueRequest {
                    status: IssueStatus::InProgress,
                    assignee: Some(dev),
                    ..new_issue("work nobody can afford")
                },
            )
            .await
            .expect("create");
        assert_eq!(
            f.manager.list_runs(&project.id, 1).await.expect("runs")[0].status,
            RunStatus::Held,
            "the board is broke, so the run is owed rather than started"
        );

        if called_off {
            f.manager
                .update_issue(
                    &project.id,
                    1,
                    IssueActor::User,
                    IssueUpdate {
                        cancelled: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .expect("cancel it");
        } else {
            f.manager
                .move_issue(&project.id, 1, IssueActor::User, IssueStatus::Done, &[1])
                .await
                .expect("finish it by hand");
        }

        f.dispatched.lock().clear();
        f.manager
            .update_project(
                &project.id,
                ProjectUpdate {
                    name: project.name.clone(),
                    description: String::new(),
                    daily_budget: Some(baybo_model::MicroUsd::from_micros(5_000_000)),
                },
            )
            .await
            .expect("a new day's ceiling");

        assert!(
            f.dispatched.lock().is_empty(),
            "called_off={called_off}: a run was dispatched on a card the board is done with"
        );
        let runs = f.manager.list_runs(&project.id, 1).await.expect("runs");
        assert_eq!(
            runs[0].status,
            RunStatus::Cancelled,
            "called_off={called_off}"
        );
        assert!(
            f.manager
                .timeline(&project.id, 1)
                .await
                .expect("timeline")
                .iter()
                .any(|e| matches!(
                    e.body,
                    baybo_store::project::IssueEventBody::RunSettled {
                        status: RunStatus::Cancelled,
                        ..
                    }
                )),
            "called_off={called_off}: and the card says the run ended"
        );
        assert!(
            !f.manager
                .timeline(&project.id, 1)
                .await
                .expect("timeline")
                .iter()
                .any(|e| matches!(
                    e.body,
                    baybo_store::project::IssueEventBody::BudgetRestored { .. }
                )),
            "called_off={called_off}"
        );
    }
}

#[tokio::test]
async fn the_boot_sweep_leaves_a_hold_held_while_the_board_is_still_broke() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Still Skint")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("held across a restart")
            },
        )
        .await
        .expect("create")
        .into_issue();
    f.dispatched.lock().clear();

    let resumed = f.manager.resume_unsettled_runs().await.expect("sweep");
    assert_eq!(resumed, 0, "a hold is not an orphan to roll forward");
    assert!(f.dispatched.lock().is_empty());
    let runs = f.manager.list_runs(&project.id, 1).await.expect("runs");
    assert_eq!(runs[0].status, RunStatus::Held);
}

#[tokio::test]
async fn a_board_with_no_ceiling_is_never_held() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Unlimited"))
        .await
        .expect("p");
    assert_eq!(project.daily_budget, None);
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("free rein")
            },
        )
        .await
        .expect("create")
        .into_issue();
    assert_eq!(f.dispatched.lock().len(), 1);
    assert_eq!(
        f.manager.list_runs(&project.id, 1).await.expect("runs")[0].status,
        RunStatus::Queued
    );
}

#[tokio::test]
async fn finishing_a_stage_wakes_the_parent_once() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Stages"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();

    let parent = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(lead.clone()),
                status: IssueStatus::Todo,
                ..new_issue("ship the thing")
            },
        )
        .await
        .expect("parent")
        .into_issue();
    for (title, stage) in [("design", 0), ("review the design", 0), ("build", 1)] {
        f.manager
            .create_issue(
                &p.id,
                IssueActor::User,
                NewIssueRequest {
                    parent: Some(parent.number),
                    stage,
                    ..new_issue(title)
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{title}: {e}"))
            .into_issue();
    }
    f.dispatched.lock().clear();

    f.manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Done, &[2])
        .await
        .expect("finish #2");
    assert!(
        f.dispatched.lock().is_empty(),
        "stage 0 still has a step left"
    );

    f.manager
        .move_issue(&p.id, 3, IssueActor::User, IssueStatus::Done, &[3, 2])
        .await
        .expect("finish #3");
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "stage 0 emptied, so the parent woke"
    );
    assert_eq!(f.dispatched.lock()[0].number, parent.number);
    assert_eq!(f.dispatched.lock()[0].trigger, RunTrigger::StageBarrier);
    assert!(
        f.manager
            .timeline(&p.id, parent.number)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(
                e.body,
                baybo_store::project::IssueEventBody::StageCompleted { stage: 0 }
            )),
        "and the parent's card says which stage opened"
    );

    f.manager
        .cancel_run(&p.id, parent.number)
        .await
        .expect("settle the parent's run");
    f.dispatched.lock().clear();
    f.manager
        .update_issue(
            &p.id,
            3,
            IssueActor::User,
            IssueUpdate {
                title: Some("review the design (again)".into()),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect("retitle");
    assert!(
        f.dispatched.lock().is_empty(),
        "the barrier fires on the transition into Done, not on every save of a Done step"
    );
}

#[tokio::test]
async fn a_cancelled_parent_is_not_woken_by_its_last_step() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Barrier"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();
    let parent = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(lead),
                status: IssueStatus::Todo,
                ..new_issue("ship the thing")
            },
        )
        .await
        .expect("parent")
        .into_issue();
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                parent: Some(parent.number),
                stage: 0,
                ..new_issue("design")
            },
        )
        .await
        .expect("child");
    f.manager
        .update_issue(
            &p.id,
            parent.number,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("call the plan off");
    f.dispatched.lock().clear();

    f.manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Done, &[2])
        .await
        .expect("close the last step anyway");
    assert!(
        f.dispatched.lock().is_empty(),
        "a cancelled parent was woken by its stage emptying"
    );
    assert!(
        f.manager
            .list_runs(&p.id, parent.number)
            .await
            .expect("runs")
            .is_empty(),
        "and no ledger row was written for it either"
    );
    assert!(
        f.manager
            .timeline(&p.id, parent.number)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(
                e.body,
                baybo_store::project::IssueEventBody::StageCompleted { stage: 0 }
            ))
    );
}

#[tokio::test]
async fn a_cancelled_step_opens_its_stage() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Cancelling"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();
    let parent = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(lead),
                status: IssueStatus::Todo,
                ..new_issue("parent")
            },
        )
        .await
        .expect("parent")
        .into_issue();
    for title in ["will happen", "will not"] {
        f.manager
            .create_issue(
                &p.id,
                IssueActor::User,
                NewIssueRequest {
                    parent: Some(parent.number),
                    stage: 0,
                    ..new_issue(title)
                },
            )
            .await
            .expect("child")
            .into_issue();
    }
    f.manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Done, &[2])
        .await
        .expect("finish #2");
    f.dispatched.lock().clear();

    f.manager
        .update_issue(
            &p.id,
            3,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect("cancel #3");
    assert_eq!(f.dispatched.lock().len(), 1, "the stage opened");
    assert_eq!(f.dispatched.lock()[0].trigger, RunTrigger::StageBarrier);
}

#[tokio::test]
async fn a_later_stage_finishing_early_is_announced_but_wakes_nobody() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Out of order"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();
    let parent = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(lead),
                status: IssueStatus::Todo,
                ..new_issue("ship the thing")
            },
        )
        .await
        .expect("parent")
        .into_issue();
    for (title, stage) in [("design", 0), ("write the release note", 1)] {
        f.manager
            .create_issue(
                &p.id,
                IssueActor::User,
                NewIssueRequest {
                    parent: Some(parent.number),
                    stage,
                    ..new_issue(title)
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{title}: {e}"))
            .into_issue();
    }
    f.dispatched.lock().clear();

    f.manager
        .move_issue(&p.id, 3, IssueActor::User, IssueStatus::Done, &[3])
        .await
        .expect("finish #3");
    assert!(
        f.dispatched.lock().is_empty(),
        "stage 1 emptied, but the board is still on stage 0"
    );
    let stages_announced = async |number: i64| -> Vec<i64> {
        f.manager
            .timeline(&p.id, number)
            .await
            .expect("timeline")
            .iter()
            .filter_map(|e| match e.body {
                baybo_store::project::IssueEventBody::StageCompleted { stage } => Some(stage),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        stages_announced(parent.number).await,
        vec![1],
        "but the card still says stage 1 closed — it did, and if the entry \
         waited for the barrier the operator would never hear about it"
    );

    f.manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Done, &[2, 3])
        .await
        .expect("finish #2");
    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1, "the parent woke exactly once");
    assert_eq!(announced[0].number, parent.number);
    assert_eq!(announced[0].trigger, RunTrigger::StageBarrier);
    assert_eq!(
        stages_announced(parent.number).await,
        vec![1, 0],
        "and each stage is named once, in the order it actually closed"
    );

    f.manager
        .update_issue(
            &p.id,
            3,
            IssueActor::User,
            IssueUpdate {
                title: Some("write the release note (final)".into()),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect("retitle the finished step");
    assert_eq!(
        stages_announced(parent.number).await,
        vec![1, 0],
        "a stage announces once per completion"
    );
}

#[tokio::test]
async fn sub_issues_do_not_nest() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Flat"))
        .await
        .expect("p");
    let parent = f
        .manager
        .create_issue(&p.id, IssueActor::User, new_issue("parent"))
        .await
        .expect("parent")
        .into_issue();
    let child = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                parent: Some(parent.number),
                ..new_issue("child")
            },
        )
        .await
        .expect("child")
        .into_issue();

    let refused = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                parent: Some(child.number),
                ..new_issue("grandchild")
            },
        )
        .await
        .expect_err("a step cannot have steps");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    let refused = f
        .manager
        .update_issue(
            &p.id,
            parent.number,
            IssueActor::User,
            IssueUpdate {
                parent: Some(Some(child.id.clone())),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect_err("a parent cannot become a child");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    let refused = f
        .manager
        .update_issue(
            &p.id,
            child.number,
            IssueActor::User,
            IssueUpdate {
                parent: Some(Some(child.id.clone())),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect_err("self-parenting");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_parent_from_another_board_does_not_resolve() {
    let f = fixture().await;
    let mine = f
        .manager
        .create_project(new_project("mine"))
        .await
        .expect("p");
    let theirs = f
        .manager
        .create_project(new_project("theirs"))
        .await
        .expect("p");
    let outsider = f
        .manager
        .create_issue(&theirs.id, IssueActor::User, new_issue("their card"))
        .await
        .expect("their issue")
        .into_issue();
    f.manager
        .create_issue(&mine.id, IssueActor::User, new_issue("my card"))
        .await
        .expect("my issue")
        .into_issue();

    let refused = f
        .manager
        .update_issue(
            &mine.id,
            1,
            IssueActor::User,
            IssueUpdate {
                parent: Some(Some(outsider.id)),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect_err("another board's card is not a parent here");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_move_must_name_its_whole_destination_column() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Ranks"))
        .await
        .expect("p");
    for title in ["one", "two", "three"] {
        f.manager
            .create_issue(&p.id, IssueActor::User, new_issue(title))
            .await
            .expect("issue")
            .into_issue();
    }
    f.manager
        .update_issue(
            &p.id,
            2,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect("cancel");

    let refused = f
        .manager
        .move_issue(&p.id, 3, IssueActor::User, IssueStatus::Backlog, &[3, 1])
        .await
        .expect_err("a list that omits #2 does not describe this column");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    let mut ranks: Vec<(i64, i64)> = f
        .manager
        .list_issues(&p.id)
        .await
        .expect("issues")
        .into_iter()
        .map(|issue| (issue.number, issue.position))
        .collect();
    ranks.sort();
    assert_eq!(ranks, vec![(1, 0), (2, 1), (3, 2)]);

    f.manager
        .move_issue(&p.id, 3, IssueActor::User, IssueStatus::Backlog, &[3, 1, 2])
        .await
        .expect("the whole column is a valid order");
    let mut ranks: Vec<(i64, i64)> = f
        .manager
        .list_issues(&p.id)
        .await
        .expect("issues")
        .into_iter()
        .map(|issue| (issue.number, issue.position))
        .collect();
    ranks.sort();
    assert_eq!(
        ranks,
        vec![(1, 1), (2, 2), (3, 0)],
        "dense and collision-free"
    );
}

#[tokio::test]
async fn a_cross_column_move_names_the_destination_plus_the_card() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Across"))
        .await
        .expect("p");
    for title in ["stays", "moves"] {
        f.manager
            .create_issue(&p.id, IssueActor::User, new_issue(title))
            .await
            .expect("issue")
            .into_issue();
    }
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("todo was empty");

    let refused = f
        .manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Todo, &[2])
        .await
        .expect_err("the list omits the card already in Todo");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    f.manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Todo, &[2, 1])
        .await
        .expect("both named");
}

#[tokio::test]
async fn the_attention_count_is_what_only_the_operator_can_clear() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Stuck")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;

    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty()
    );

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("cannot afford this")
            },
        )
        .await
        .expect("create")
        .into_issue();
    let counts = f.manager.attention(&[]).await.expect("attention");
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].1.held, 1);
    assert_eq!(counts[0].1.failed, 0);

    f.manager
        .update_project(
            &p.id,
            ProjectUpdate {
                name: p.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(5_000_000)),
            },
        )
        .await
        .expect("raise");
    let counts = f.manager.attention(&[]).await.expect("attention");
    assert!(
        counts.iter().all(|(_, c)| c.held == 0),
        "the hold was released: {counts:?}"
    );
}

#[tokio::test]
async fn a_failed_run_stops_counting_once_somebody_acts_on_it() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Failing"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("went wrong")
            },
        )
        .await
        .expect("create")
        .into_issue();
    let run = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store_settle(&run, RunStatus::Failed).await;

    let counts = f.manager.attention(&[]).await.expect("attention");
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].1.failed, 1);

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(Some("waiting on upstream".to_owned())),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect("block");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty()
    );

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(None),
                ..IssueUpdate::default()
            },
        )
        .await
        .expect("unblock");
    assert_eq!(
        f.manager.attention(&[]).await.expect("attention")[0]
            .1
            .failed,
        1
    );
    f.manager.retry_run(&p.id, 1).await.expect("retry");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty(),
        "the newest run is queued, so nothing is asking for a person"
    );
}

#[tokio::test]
async fn an_archived_board_asks_for_nothing() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            ..new_project("Shelved")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("held")
            },
        )
        .await
        .expect("create")
        .into_issue();
    assert_eq!(f.manager.attention(&[]).await.expect("attention").len(), 1);

    f.manager
        .set_project_archived(&p.id, true)
        .await
        .expect("archive");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty()
    );
}

#[tokio::test]
async fn a_boards_unread_count_is_what_happened_since_you_looked() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Unread"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                status: IssueStatus::Todo,
                ..new_issue("work")
            },
        )
        .await
        .expect("create")
        .into_issue();

    f.manager
        .comment(&p.id, 1, IssueActor::User, "any progress?")
        .await
        .expect("comment");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty(),
        "your own words are not unread"
    );

    f.manager
        .comment(
            &p.id,
            1,
            IssueActor::Agent(dev.clone()),
            "blocked on the API",
        )
        .await
        .expect("comment");
    let counts = f.manager.attention(&[]).await.expect("attention");
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].1.unread, 1);

    f.manager
        .move_issue(&p.id, 1, IssueActor::Agent(dev), IssueStatus::Review, &[1])
        .await
        .expect("move to review");
    assert_eq!(
        f.manager.attention(&[]).await.expect("attention")[0]
            .1
            .unread,
        2
    );

    f.manager.mark_read(&p.id).await.expect("mark read");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty()
    );
}

#[tokio::test]
async fn the_boards_own_traffic_is_never_unread() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Noise"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager.mark_read(&p.id).await.expect("mark read");

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                status: IssueStatus::InProgress,
                ..new_issue("in flight")
            },
        )
        .await
        .expect("create")
        .into_issue();
    f.manager
        .move_issue(&p.id, 1, IssueActor::Agent(dev), IssueStatus::Done, &[1])
        .await
        .expect("finish");

    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .iter()
            .all(|(_, c)| c.unread == 0),
        "run and column traffic is not something waiting on a person"
    );
}

#[tokio::test]
async fn a_mention_on_an_unowned_card_hands_it_over() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Mentions"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                ..new_issue("nobody is on this")
            },
        )
        .await
        .expect_err("In Progress needs an assignee");
    f.manager
        .create_issue(&p.id, IssueActor::User, new_issue("nobody is on this"))
        .await
        .expect("create")
        .into_issue();
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("to todo");
    f.dispatched.lock().clear();

    f.manager
        .comment(&p.id, 1, IssueActor::User, "@dev-1 could you take this?")
        .await
        .expect("comment");

    let issue = f.manager.get_issue(&p.id, 1).await.expect("issue");
    assert_eq!(issue.assignee.as_ref(), Some(&dev));
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "assigned into a live column, so it starts — one run, not two"
    );
    assert!(
        f.manager
            .timeline(&p.id, 1)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(
                &e.body,
                baybo_store::project::IssueEventBody::Assigned { to: Some(to), .. } if to == &dev
            ))
    );
}

#[tokio::test]
async fn a_mention_hands_the_card_over_in_the_commenters_name() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Delegating"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(&p.id, IssueActor::User, new_issue("nobody is on this"))
        .await
        .expect("create")
        .into_issue();
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("to todo");

    f.manager
        .comment(
            &p.id,
            1,
            IssueActor::Agent(lead.clone()),
            "@dev-1 take this",
        )
        .await
        .expect("comment");

    assert_eq!(
        f.manager.get_issue(&p.id, 1).await.expect("issue").assignee,
        Some(dev.clone())
    );
    let timeline = f.manager.timeline(&p.id, 1).await.expect("timeline");
    let comment = timeline
        .iter()
        .find(|e| {
            matches!(
                &e.body,
                baybo_store::project::IssueEventBody::Comment { .. }
            )
        })
        .expect("the comment is on the timeline");
    let assigned = timeline
        .iter()
        .find(|e| {
            matches!(
                &e.body,
                baybo_store::project::IssueEventBody::Assigned { to: Some(to), .. } if to == &dev
            )
        })
        .expect("and so is the handover it caused");
    assert_eq!(comment.actor, IssueActor::Agent(lead.clone()));
    assert_eq!(
        assigned.actor,
        IssueActor::Agent(lead),
        "the two entries agree on who did it — the lead said it and the lead \
         did it, not 'you assigned it to @dev-1'"
    );
}

#[tokio::test]
async fn a_mention_never_takes_a_card_off_its_owner() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Asking"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let other = seed_agent(&f, &p.id, "dev-2", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                status: IssueStatus::Todo,
                ..new_issue("owned")
            },
        )
        .await
        .expect("create")
        .into_issue();

    f.manager
        .comment(&p.id, 1, IssueActor::User, "@dev-2 what do you think?")
        .await
        .expect("comment");
    let issue = f.manager.get_issue(&p.id, 1).await.expect("issue");
    assert_eq!(issue.assignee.as_ref(), Some(&dev), "still @dev-1's card");
    assert_ne!(issue.assignee.as_ref(), Some(&other));
}

#[tokio::test]
async fn a_mention_of_a_stranger_still_records_the_comment() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Typos"))
        .await
        .expect("p");
    f.manager
        .create_issue(&p.id, IssueActor::User, new_issue("unowned"))
        .await
        .expect("create")
        .into_issue();

    f.manager
        .comment(&p.id, 1, IssueActor::User, "@nobody-here please look")
        .await
        .expect("a typo in a mention is not a reason to lose the comment");
    assert!(
        f.manager
            .get_issue(&p.id, 1)
            .await
            .expect("issue")
            .assignee
            .is_none()
    );
    assert!(
        f.manager
            .timeline(&p.id, 1)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(&e.body, baybo_store::project::IssueEventBody::Comment { text } if text.contains("nobody-here")))
    );
}

#[tokio::test]
async fn a_deferred_wake_starts_the_run_the_comment_asked_for() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Deferred"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let issue = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("somebody spoke mid-run")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    let first = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store_settle(&first, RunStatus::Done).await;
    f.dispatched.lock().clear();

    let woken = f
        .manager
        .wake_on_comment(&p.id, issue.number)
        .await
        .expect("the issue is still listening");
    assert_eq!(woken.trigger, RunTrigger::Comment);
    assert_eq!(woken.attempt, 2);
    let announced = f.dispatched.lock().clone();
    assert_eq!(
        announced.len(),
        1,
        "and it reached the dispatcher, like every other start"
    );
    assert_eq!(announced[0].id, woken.id);
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].status,
        RunStatus::Queued,
        "not a row nothing will ever pick up"
    );
}

#[tokio::test]
async fn a_wake_after_a_run_is_refused_once_the_card_leaves_live_work() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Finished"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let issue = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("done by the time anybody replied")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    let first = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store_settle(&first, RunStatus::Done).await;
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Done, &[1])
        .await
        .expect("to done");
    f.dispatched.lock().clear();

    assert!(
        f.manager
            .wake_on_comment(&p.id, issue.number)
            .await
            .is_none()
    );
    assert!(f.dispatched.lock().is_empty());
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        1,
        "and nothing was recorded to hold the issue's slot"
    );
}

#[tokio::test]
async fn a_resumed_run_keeps_the_session_it_was_working_in() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Interrupted"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("killed mid-turn")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    let run = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    let session = baybo_model::SessionId::from("issue-1-session");
    assert!(
        f.store.claim_run(&run, &session).await.expect("claim"),
        "the executor took it"
    );
    f.dispatched.lock().clear();

    f.manager.resume_unsettled_runs().await.expect("boot sweep");

    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].status, RunStatus::Queued);
    assert_eq!(
        announced[0].session_id.as_ref(),
        Some(&session),
        "the executor is handed the session the run was already in"
    );
}

#[tokio::test]
async fn cancelling_a_resumed_run_settles_it_rather_than_chasing_a_dead_turn() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Cancel after boot"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("killed mid-turn")
            },
        )
        .await
        .expect("issue")
        .into_issue();
    let run = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store
        .claim_run(&run, &baybo_model::SessionId::from("issue-1-session"))
        .await
        .expect("claim");
    f.manager.resume_unsettled_runs().await.expect("boot sweep");

    assert!(
        f.manager
            .cancel_run(&p.id, 1)
            .await
            .expect("cancel")
            .is_none(),
        "a queued run has no live turn to stop, session or no session"
    );
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].status,
        RunStatus::Cancelled
    );
}

/// A board with one card in progress, its first run recorded and claimed —
/// the state an executor is handed.
async fn card_being_worked() -> (Fixture, ProjectRow, AgentProfileId, IssueRunRow) {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("wire the importer")
            },
        )
        .await
        .expect("issue");
    let run = f.dispatched.lock()[0].clone();
    assert!(
        f.manager
            .start_run(&run, &SessionId::from("sess-1"))
            .await
            .expect("claim"),
        "the first executor to arrive takes it"
    );
    f.dispatched.lock().clear();
    (f, p, dev, run)
}

fn settled_entries(timeline: &[IssueEventRow]) -> Vec<&IssueEventRow> {
    timeline
        .iter()
        .filter(|e| {
            matches!(
                e.body,
                baybo_store::project::IssueEventBody::RunSettled { .. }
            )
        })
        .collect()
}

/// The checkout the executor hands back. Never created, so there is no
/// branch to surface — which is what every test here wants except the one
/// about surfacing branches.
fn no_checkout() -> &'static std::path::Path {
    std::path::Path::new("/nonexistent/checkout")
}

fn done() -> baybo_project::RunOutcome {
    baybo_project::RunOutcome {
        status: RunStatus::Done,
        error: None,
        stopped_by_a_human: false,
    }
}

/// Close a run out the way the waiter does. `briefed_at` is the instant the
/// run's brief was read, which the dispatcher stamps in production — every
/// test names its own, because which side of it a comment falls on is the
/// rule being tested.
async fn finish(
    f: &Fixture,
    run: &IssueRunRow,
    briefed_at: DateTime<Utc>,
    outcome: baybo_project::RunOutcome,
) {
    f.manager
        .finish_run(run, no_checkout(), briefed_at, outcome)
        .await;
}

#[tokio::test]
async fn cancelling_a_run_that_never_started_says_so_on_the_card() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("queued and then thought better of")
            },
        )
        .await
        .expect("issue");

    assert!(
        f.manager
            .cancel_run(&p.id, 1)
            .await
            .expect("cancel")
            .is_none(),
        "a queued run is settled where it stands; there is no session to stop"
    );

    let timeline = f.manager.timeline(&p.id, 1).await.expect("timeline");
    let settled = settled_entries(&timeline);
    assert_eq!(
        settled.len(),
        1,
        "the card says the run was cancelled, the same as every other settle does:\n{timeline:#?}"
    );
    assert_eq!(
        settled[0].actor,
        IssueActor::User,
        "and says a person asked for it"
    );
}

#[tokio::test]
async fn a_settle_is_written_down_once_however_many_times_it_is_asked_for() {
    let (f, p, _dev, run) = card_being_worked().await;

    finish(&f, &run, run.created_at, done()).await;
    finish(
        &f,
        &run,
        run.created_at,
        baybo_project::RunOutcome {
            status: RunStatus::Failed,
            error: Some("a replayed settle".to_owned()),
            stopped_by_a_human: false,
        },
    )
    .await;

    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].status,
        RunStatus::Done,
        "the first settle is the one that stands"
    );
    let timeline = f.manager.timeline(&p.id, 1).await.expect("timeline");
    assert_eq!(
        settled_entries(&timeline).len(),
        1,
        "and a replay puts nothing second on the timeline:\n{timeline:#?}"
    );
}

#[tokio::test]
async fn a_comment_the_run_was_already_told_about_does_not_start_a_second_one() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("wire the importer")
            },
        )
        .await
        .expect("issue");
    let run = f.dispatched.lock()[0].clone();

    // Said while the run was still queued — a held run can sit there a day.
    // The dispatcher reads the brief afterwards, so this is in it.
    f.manager
        .comment(&p.id, 1, IssueActor::User, "start with the CSV path")
        .await
        .expect("comment");
    f.dispatched.lock().clear();
    let briefed_at = Utc::now();

    assert!(
        f.manager
            .start_run(&run, &SessionId::from("sess-1"))
            .await
            .expect("claim"),
        "and only then does an executor pick it up"
    );
    finish(&f, &run, briefed_at, done()).await;

    assert!(
        f.dispatched.lock().is_empty(),
        "the run was handed this comment; going back for it would ask for the work twice"
    );
}

#[tokio::test]
async fn a_comment_left_while_the_checkout_was_being_cut_is_still_picked_up() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("wire the importer")
            },
        )
        .await
        .expect("issue");
    let run = f.dispatched.lock()[0].clone();

    // The gap neither instant on the ledger row can see. The brief has been
    // read; the dispatcher is still inside `git worktree add`, and the run
    // has not crossed the channel to be claimed. Backdated a millisecond so
    // the ordering is the clock's to prove, not the scheduler's.
    let briefed_at = Utc::now() - chrono::Duration::milliseconds(1);
    f.manager
        .comment(&p.id, 1, IssueActor::User, "also handle the empty case")
        .await
        .expect("comment");
    f.dispatched.lock().clear();

    // Only now does the executor pick it up, so `started_at` falls on the
    // far side of the comment.
    assert!(
        f.manager
            .start_run(&run, &SessionId::from("sess-1"))
            .await
            .expect("claim"),
        "the executor claims after the comment landed"
    );
    finish(&f, &run, briefed_at, done()).await;

    let dispatched = f.dispatched.lock().clone();
    assert_eq!(
        dispatched.len(),
        1,
        "the run was never told this, and `comment_delivery` promised the \
         operator it would be — a window bounded by the claim drops it and \
         nothing else comes back for it"
    );
    assert_eq!(dispatched[0].trigger, RunTrigger::Comment);
}

#[tokio::test]
async fn a_run_does_not_wake_itself_on_its_own_progress_note() {
    let (f, p, dev, run) = card_being_worked().await;

    f.manager
        .comment(
            &p.id,
            1,
            IssueActor::Agent(dev.clone()),
            "halfway through the importer",
        )
        .await
        .expect("the run says where it has got to");

    finish(&f, &run, run.created_at, done()).await;

    assert!(
        f.dispatched.lock().is_empty(),
        "an agent reporting progress is not somebody asking it for more"
    );
}

#[tokio::test]
async fn somebody_elses_comment_during_a_run_does_start_a_follow_up() {
    let (f, p, _dev, run) = card_being_worked().await;

    f.manager
        .comment(&p.id, 1, IssueActor::User, "also handle the empty case")
        .await
        .expect("comment");

    finish(&f, &run, run.created_at, done()).await;

    let dispatched = f.dispatched.lock().clone();
    assert_eq!(dispatched.len(), 1, "the card is picked back up");
    assert_eq!(dispatched[0].trigger, RunTrigger::Comment);
}

#[tokio::test]
async fn a_run_somebody_stopped_is_left_stopped() {
    let (f, p, _dev, run) = card_being_worked().await;

    f.manager
        .comment(&p.id, 1, IssueActor::User, "actually, stop")
        .await
        .expect("comment");

    finish(
        &f,
        &run,
        run.created_at,
        baybo_project::RunOutcome {
            status: RunStatus::Cancelled,
            error: None,
            stopped_by_a_human: true,
        },
    )
    .await;

    assert!(
        f.dispatched.lock().is_empty(),
        "somebody who pressed stop is not asking for the work to continue"
    );
}

async fn git(dir: &std::path::Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn a_card_finished_before_its_run_settles_still_surfaces_the_branch() {
    let (f, p, _dev, run) = card_being_worked().await;

    let root = baybo_project::worktree::worktree_root(&f.paths, &p.id, 1);
    let branch = baybo_project::worktree::branch_name(1, "wire the importer");
    baybo_project::worktree::ensure(std::path::Path::new(&p.workdir), &root, &branch)
        .await
        .expect("cut the worktree a run works in");
    tokio::fs::write(root.join("importer.rs"), "fn main() {}")
        .await
        .expect("the run's work");
    git(&root, &["add", "."]).await;
    git(
        &root,
        &[
            "-c",
            "user.name=dev-1",
            "-c",
            "user.email=dev-1@baybo.local",
            "commit",
            "-qm",
            "wire the importer",
        ],
    )
    .await;

    // The card is finished while the run is still in flight — the assignee
    // closing its own card, or an operator doing it. That reclaims the
    // checkout out from under the run that is about to settle.
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Done, &[1])
        .await
        .expect("finish it");
    assert!(!root.exists(), "the checkout was reclaimed");

    f.manager
        .finish_run(&run, &root, run.created_at, done())
        .await;

    assert_eq!(
        f.manager
            .get_issue(&p.id, 1)
            .await
            .expect("issue")
            .branch
            .as_deref(),
        Some(branch.as_str()),
        "the board never merges, so the branch is the one artefact it hands over"
    );
}

/// A hook that reports what the board announced, in order. The channel is
/// what lets a test await a dispatcher's own background task without
/// sleeping on it.
struct RecordingEvents(tokio::sync::mpsc::UnboundedSender<String>);

impl baybo_project::ProjectEvents for RecordingEvents {
    fn project_changed(&self, project: &ProjectId) {
        let _ = self.0.send(format!("project {project}"));
    }
    fn board_changed(&self, project: &ProjectId, issue: Option<i64>) {
        let _ = self.0.send(format!("board {project} {issue:?}"));
    }
    fn run_changed(&self, project: &ProjectId, issue: i64) {
        let _ = self.0.send(format!("run {project} #{issue}"));
    }
    fn timeline_changed(&self, project: &ProjectId, issue: i64) {
        let _ = self.0.send(format!("timeline {project} #{issue}"));
    }
}

#[tokio::test]
async fn a_run_whose_checkout_cannot_be_cut_says_so_instead_of_shimmering() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("wire the importer")
            },
        )
        .await
        .expect("issue");
    let run = f.dispatched.lock()[0].clone();

    // The repository the card's checkout would be cut from is gone.
    tokio::fs::remove_dir_all(&p.workdir)
        .await
        .expect("take the repository away");

    let (tx, mut announced) = tokio::sync::mpsc::unbounded_channel();
    let (dispatch, _rx) = baybo_project::dispatch::build(baybo_project::DispatchConfig {
        store: Arc::clone(&f.store),
        events: Arc::new(RecordingEvents(tx)),
        paths: f.paths.clone(),
        user_id: "owner".to_owned(),
        channel: baybo_model::ChannelType::owner(),
    });
    dispatch(run.clone());

    // The dispatcher prepares on its own task; its first announcement is
    // what says it has got as far as settling.
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), announced.recv())
        .await
        .expect("the board is told, rather than left to work it out")
        .expect("hook is live");
    assert_eq!(first, format!("run {} #1", p.id));

    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    assert_eq!(runs[0].status, RunStatus::Failed);
    let timeline = f.manager.timeline(&p.id, 1).await.expect("timeline");
    let settled = settled_entries(&timeline);
    assert_eq!(
        settled.len(),
        1,
        "and the card says why, not just that it stopped:\n{timeline:#?}"
    );
    assert_eq!(settled[0].actor, IssueActor::System);
}
