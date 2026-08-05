//! Domain-level tests for [`ProjectManager`]: what a write has to satisfy
//! before it lands, and the workdir guard that keeps a project's checkout
//! out of baybo's own workspace.

use std::sync::Arc;

use baybo_model::{AgentFramework, AgentHandle, AgentProfileId, ProjectId, TeamMembership};
use baybo_project::{NewIssueRequest, NewProject, ProjectError, ProjectManager};
use baybo_store::project::{
    IssueActor, IssuePriority, IssueRunRow, IssueStatus, IssueUpdate, ProjectUpdate, RunStatus,
    RunTrigger,
};
use baybo_workspace::WorkspacePaths;

struct Fixture {
    manager: ProjectManager,
    /// The raw store, for the few facts a test has to arrange that no
    /// public method produces — a run that failed, which only an executor
    /// settles.
    store: Arc<dyn baybo_store::project::ProjectStore>,
    /// Every run the manager announced, in order — what an executor would
    /// have been handed.
    dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>>,
    agents: Arc<dyn baybo_store::AgentProfileStore>,
    paths: WorkspacePaths,
    _workspace: tempfile::TempDir,
}

/// Put an agent on a project's team. Assignees have to be teammates, so
/// every fixture that assigns work goes through here.
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
    /// Settle a run the way the waiter would. There is no public path: the
    /// manager records runs and an executor settles them.
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

/// Every board opens with a coordinator, so no reader — the team strip, the
/// triage loop, the chat panel — has to handle a project nobody is on.
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

    // Its persona is on disk with the coordinator soul and a name, so the
    // first run reads a lead rather than the blank custom-agent skeleton.
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

    // And it is a teammate, not a chat persona: the global roster is
    // exactly what it was before the project existed.
    let global = f.agents.list().await.expect("global roster");
    assert!(
        global.iter().all(|row| row.id != lead.id),
        "the lead must not appear in the global agent list"
    );

    // Two boards get two leads, each answering to `@lead` on its own.
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

/// The lead is a teammate like any other, which is what makes it usable as
/// an assignee without a special case anywhere.
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
    // Still nobody's chat persona.
    assert!(
        f.agents
            .list()
            .await
            .expect("global")
            .iter()
            .all(|row| row.id != hired.id)
    );
}

/// Handles stay reserved after a removal, so a second "QA" cannot simply
/// take `@qa` back — it gets numbered rather than refused or, worse,
/// silently inheriting the departed agent's mentions.
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

    // Even after the first one leaves: its timeline entries still say @qa.
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

    // The board already has its lead, so the cap allows one fewer hire.
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

    // Cancel the run and the removal goes through.
    f.manager.cancel_run(&p.id, 1).await.expect("cancel");
    f.manager
        .remove_from_team(&p.id, &dev.id)
        .await
        .expect("remove");
    assert_eq!(f.manager.team(&p.id).await.expect("team").len(), 1);
    // Twice is refused rather than silently accepted.
    assert!(f.manager.remove_from_team(&p.id, &dev.id).await.is_err());
}

/// Another board's agent is not this board's to remove, even though the row
/// resolves and the tombstone would happily be written.
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
    // `work/` is shared with the bash tool's scratch space; something is
    // already sitting where this project's name would put it.
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

    // The same directory, once it is a repo, is accepted.
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
    // `state/` holds baybo's own database. Binding it read-write into every
    // shell the team runs is the whole reason this guard exists.
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
    // The ancestor direction: the default workspace lives in `~/.baybo`, so
    // `workdir = $HOME` passes any descendant-only check and then binds the
    // whole home directory.
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
    // The sandbox resolves symlinks when it mounts, so a lexical-only check
    // would pass this link and then bind exactly what it refused.
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

    // Reading still works — that is what makes archive different from delete.
    let issues = f.manager.list_issues(&project.id).await.expect("list");
    assert_eq!(issues.len(), 1);

    // And it comes back.
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

    // Both projects have a #1, and each one only has its own.
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

    // A whitespace-only block reason reads as an unblock, not a blank block.
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

    // `ordered_numbers` is the destination column's new contents, so a list
    // that omits the moved card would leave it unplaced.
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

    // The board would otherwise claim work is under way that nobody is doing.
    let refused = f
        .manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::InProgress, &[1])
        .await
        .expect_err("an unassigned card cannot start");
    assert!(
        matches!(refused, ProjectError::Invalid { .. }),
        "{refused:?}"
    );

    // Every other column is free to be unassigned.
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("todo takes unassigned work");

    // Assigned, it moves.
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

    // …and cannot be abandoned mid-flight.
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

    // An external profile has no top-level session leg, so a card assigned
    // to one could never start. Refused at the door rather than at run time.
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

/// The roster is the assignable set. A global chat persona is a real,
/// runnable baybo agent — and still not assignable here, because it has no
/// handle on this board and appears in none of its surfaces.
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

/// A removed teammate still resolves — that is what keeps the timeline
/// readable — so "the row exists" is not the question the assign path asks.
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

    // The row exists before anything could have acted on it — that is what
    // makes a crash here recoverable rather than a run that never happened.
    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Queued);
    assert_eq!(runs[0].trigger, RunTrigger::Started);
    assert_eq!(runs[0].agent_id, dev);

    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1, "and exactly one run was handed out");
    assert_eq!(announced[0].id, runs[0].id);

    // Dragging within the column, or out of it, starts nothing more — and
    // in particular leaving the column does not stop the run.
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

    // Straight into In Progress at creation: one edge.
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

    // Reassigning while a run is in flight does not start a second one:
    // the dedupe guard is a unique index, so it holds even though nothing
    // here checked first.
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

    // Nothing settled the run — the process died. Boot finds it.
    let resumed = f.manager.resume_unsettled_runs().await.expect("boot sweep");
    assert_eq!(resumed, 1);
    let announced = f.dispatched.lock().clone();
    assert_eq!(announced.len(), 1, "and hands it back out to be executed");
    assert_eq!(announced[0].status, RunStatus::Queued);
}

#[tokio::test]
async fn the_board_writes_its_own_history() {
    // The end-to-end claim of the timeline: work the operator does through
    // the board leaves a readable trail without anybody being asked to
    // write one.
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
    // The board's promise: saying something to somebody who is on the job
    // gets you an agent. Saying something into the backlog gets you a note.
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
    // The queued run assembles its brief when it starts, so it reads this
    // itself. A second run would be two agents on one card — and the
    // dedupe index would refuse it, losing the wake entirely.
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
    // Creating it in In Progress already queued a run.
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
    // Reclamation is the only thing that ever removes a worktree — nothing
    // sweeps them — so an issue that reaches Done and keeps its checkout
    // forever is a disk leak with no other backstop.
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
    // The checkout is the only copy of whatever the agent had not
    // committed. Reaching Done is a statement about the work, not
    // permission to delete it.
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

/// The gate's shape: the run is **recorded** and then held, so an
/// exhausted board owes work rather than dropping it.
#[tokio::test]
async fn a_board_over_budget_records_the_work_it_is_not_doing() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            // Zero is how an operator pauses a board without archiving it.
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

    // And the card says why, in figures.
    let timeline = f.manager.timeline(&project.id, 1).await.expect("timeline");
    let held = timeline
        .iter()
        .find_map(|e| match &e.body {
            baybo_store::project::IssueEventBody::BudgetExhausted {
                spent_micros,
                limit_micros,
            } => Some((*spent_micros, *limit_micros)),
            _ => None,
        })
        .expect("the timeline says the run was held");
    assert_eq!(held, (0, 0));
}

/// Raising the ceiling starts what it was blocking, without the operator
/// touching each card.
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
            )),
        "and the card says it was released"
    );
}

/// A negative ceiling is a caller mistake, not a board to be silently
/// paused.
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

/// The boot sweep must not start held runs as if they were orphans, and
/// must re-evaluate them against the budget it finds.
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

/// No ceiling is the default, and it must cost nothing and gate nothing.
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

/// The barrier: a stage opens exactly once, when the last step in it
/// finishes, and it wakes whoever is on the parent.
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

    // The first step of stage 0 finishing is not the stage finishing.
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

    // Re-saving a Done step must not wake it again. The parent's run has
    // to be settled first, or the dedupe index would refuse the second
    // enqueue and hide a barrier that fires on every save.
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

/// A cancelled step must not hold its stage open — cancelling is how an
/// operator unblocks a barrier on work nobody is going to do.
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

/// Sub-issues are one level deep, and that is enforced in both directions.
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

    // A step cannot be given steps.
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

    // …and a card that already has steps cannot become one.
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

    // Nor can an issue be its own step.
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

/// A parent id from another board must not resolve — `IssueUpdate::parent`
/// carries a ULID, so the scope check is the only thing between a request
/// and somebody else's card.
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

/// A move renumbers the destination column densely, so an `ordered_numbers`
/// that omits a card in that column leaves it holding a stale rank that
/// collides with a renumbered one — silently, because nothing downstream
/// reads `position` for anything but sorting.
///
/// The way a client gets this wrong is by sending the list it is *showing*:
/// a filtered board omits exactly the cards the operator cannot see.
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
    // #2 is cancelled — still in Backlog, still holding rank 1, and exactly
    // the kind of card a board hides.
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

    // The board is untouched: refusing beats renumbering half a column.
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

    // Naming every card in the column — cancelled ones included — works.
    f.manager
        .move_issue(&p.id, 3, IssueActor::User, IssueStatus::Backlog, &[3, 1, 2])
        .await
        .expect("the whole column is a valid order");
    // By number, because `list_issues` returns board order and the claim
    // here is about ranks, not about listing order.
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

/// The same rule across a column boundary: the destination list is the
/// column the card is arriving in, plus the card.
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

    // Todo now holds #1; moving #2 in has to name both.
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

/// The badge's contract: it counts things only the operator can unstick,
/// and each one disappears when they unstick it. No read state, so nothing
/// can be left showing a fact that is no longer true.
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

    // Nothing waiting yet: a board with no work is absent entirely, not a
    // row of zeroes.
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty()
    );

    // A run the budget held.
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

    // Raising the ceiling releases it — the operator's action, and the
    // count goes with it rather than needing to be marked read.
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

/// A failed run counts only while the card is live and nobody has parked
/// it, and only if it is the *newest* run — a retry supersedes it.
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

    // Blocking it with a reason is the operator saying "I know" — the card
    // stops asking.
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

    // Unblocking brings it back, and a retry supersedes it for good.
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

/// An archived board is read-only, so nothing on it is waiting for anybody.
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

/// The one read cursor in the feature, and what it is for: two signals
/// that leave no trace when read, so nothing derived could clear them.
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

    // The operator's own comment is not news to the operator.
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

    // An agent's is.
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

    // …and so is a card arriving in Review, which nothing else notices.
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

    // Looking at the board clears it, and nothing else does.
    f.manager.mark_read(&p.id).await.expect("mark read");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty()
    );
}

/// Ordinary board traffic is not news. This is the line between "something
/// needs you" and "the board is working", and getting it wrong makes the
/// badge permanent.
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

    // Opening a card, assigning it, starting it, moving it anywhere but
    // Review — all of it is the board doing its job.
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
