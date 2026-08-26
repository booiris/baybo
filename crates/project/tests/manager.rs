use std::sync::Arc;

use baybo_model::{
    AgentFramework, AgentHandle, AgentProfileId, IssueId, IssueRunId, ProjectId, SessionId,
    TeamMembership,
};
use baybo_project::{NewIssueRequest, NewProject, ProjectError, ProjectManager};
use baybo_store::project::{
    AttentionCounts, DEFAULT_MAX_PARALLEL_ISSUE_RUNS, IssueActor, IssueEventBody, IssueEventRow,
    IssuePriority, IssueRow, IssueRunRow, IssueStatus, IssueUpdate, NewIssue, NewIssueEvent,
    NewIssueRun, ProjectRow, ProjectUpdate, Result as StoreResult, RunStatus, RunTrigger,
};
use baybo_workspace::WorkspacePaths;
use chrono::{DateTime, Utc};

struct Fixture {
    manager: ProjectManager,
    store: Arc<dyn baybo_store::project::ProjectStore>,
    dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>>,
    stopped: Arc<parking_lot::Mutex<Vec<(SessionId, baybo_project::RunStopReason)>>>,
    agents: Arc<dyn baybo_store::AgentProfileStore>,
    blobs: Arc<dyn baybo_store::BlobStore>,
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
            llm: baybo_model::LlmPin::unpinned(),
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
    let stopped: Arc<parking_lot::Mutex<Vec<(SessionId, baybo_project::RunStopReason)>>> =
        Arc::default();
    Fixture {
        manager: ProjectManager::new(
            Arc::clone(&store.project),
            Arc::clone(&store.agent_profile),
            Arc::clone(&store.blob),
            paths.clone(),
            Arc::new(baybo_project::NoopProjectEvents),
            {
                let seen = Arc::clone(&dispatched);
                Arc::new(move |run| seen.lock().push(run))
            },
            Arc::new(RecordingStopper {
                stopped: Arc::clone(&stopped),
            }),
        ),
        agents: Arc::clone(&store.agent_profile),
        blobs: Arc::clone(&store.blob),
        store: Arc::clone(&store.project),
        dispatched,
        stopped,
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

/// A board that starts only what it is told to.
///
/// The driver is on by default in production, and Todo is where it looks —
/// so a test about anything else would otherwise have its cards promoted
/// out from under it the moment they were staffed. The driver's own tests
/// call [`driven_project`] instead.
fn new_project(name: &str) -> NewProject {
    NewProject {
        max_parallel_issue_runs: Some(0),
        ..driven_project(name)
    }
}

fn driven_project(name: &str) -> NewProject {
    NewProject {
        name: name.to_owned(),
        description: String::new(),
        workdir: None,
        daily_budget: None,
        daily_budget_tokens: None,
        max_parallel_issue_runs: None,
        agents_may_merge: false,
    }
}

fn new_issue(title: &str) -> NewIssueRequest {
    NewIssueRequest {
        title: title.to_owned(),
        description: String::new(),
        attachments: Vec::new(),
        status: IssueStatus::Backlog,
        priority: IssuePriority::None,
        assignee: None,
        parent: None,
        stage: 0,
        source_key: None,
        filed_from: None,
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
    // The lead's name is its handle too — one word for one teammate.
    assert_eq!(
        baybo_workspace::display_name(&identity).as_deref(),
        Some("lead")
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

/// A board with a real repository behind it, one card in Review, and a
/// branch with a commit on it waiting to land.
async fn board_with_a_branch_to_land(
    f: &Fixture,
    agents_may_merge: bool,
) -> (ProjectRow, IssueRow) {
    let project = f
        .manager
        .create_project(NewProject {
            agents_may_merge,
            ..new_project("Landing")
        })
        .await
        .expect("project");
    let repo = std::path::PathBuf::from(&project.workdir);
    // The materialised workdir is `git init`-fresh, and git refuses to merge
    // into an empty head, so the trunk needs its first commit.
    write_and_commit(&repo, "base.txt", "base").await;

    let issue = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Review,
                ..new_issue("land me")
            },
        )
        .await
        .expect("issue")
        .into_issue();

    let root = baybo_project::worktree::worktree_root(&f.paths, &project.id, issue.number);
    let branch = baybo_project::worktree::branch_name(issue.number, &issue.title);
    baybo_project::worktree::ensure(&repo, &root, &branch)
        .await
        .expect("worktree");
    write_and_commit(&root, "done.txt", "the work").await;
    (project, issue)
}

async fn write_and_commit(dir: &std::path::Path, name: &str, body: &str) {
    tokio::fs::write(dir.join(name), body).await.expect("write");
    for args in [
        vec!["add", "--all"],
        vec![
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "work",
        ],
    ] {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&args)
            .output()
            .await
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
}

#[tokio::test]
async fn a_board_that_does_not_merge_refuses_and_says_the_branch_is_the_handover() {
    let f = fixture().await;
    let (project, issue) = board_with_a_branch_to_land(&f, false).await;

    let merged = f
        .manager
        .merge_issue_branch(&project.id, issue.number, IssueActor::User)
        .await
        .expect("a refusal is an answer, not an error");
    let baybo_project::worktree::Merged::Refused { reason, .. } = merged else {
        panic!("a board with the setting off must refuse: {merged:?}");
    };
    assert!(
        reason.contains("do not merge"),
        "the refusal has to read as policy, not breakage: {reason}"
    );
    assert!(
        f.store
            .list_events(&issue.id)
            .await
            .expect("timeline")
            .iter()
            .all(|e| !matches!(e.body, IssueEventBody::BranchMerged { .. })),
        "a refusal writes nothing to the card"
    );
}

/// The flag removes the person who used to run the merge; it does not
/// remove the review the board already does.
#[tokio::test]
async fn a_card_nobody_has_looked_at_is_not_merged_even_on_a_merging_board() {
    let f = fixture().await;
    let (project, issue) = board_with_a_branch_to_land(&f, true).await;
    f.manager
        .move_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueStatus::Todo,
            &[issue.number],
        )
        .await
        .expect("back to todo");

    let merged = f
        .manager
        .merge_issue_branch(&project.id, issue.number, IssueActor::User)
        .await
        .expect("refusal");
    assert!(
        matches!(&merged, baybo_project::worktree::Merged::Refused { reason, .. }
            if reason.contains("review")),
        "an unreviewed card must be told to go through review: {merged:?}"
    );
}

#[tokio::test]
async fn landing_a_branch_records_it_on_the_card_and_says_where_it_went() {
    let f = fixture().await;
    let (project, issue) = board_with_a_branch_to_land(&f, true).await;
    assert_eq!(issue.branch, None, "nothing has recorded a branch yet");

    let merged = f
        .manager
        .merge_issue_branch(&project.id, issue.number, IssueActor::User)
        .await
        .expect("merge");
    let baybo_project::worktree::Merged::Landed { into, commits, .. } = merged else {
        panic!("expected a landing: {merged:?}");
    };
    assert_eq!(commits, 1);

    // Recorded *before* the merge on purpose: a merged branch is zero
    // commits ahead, and `record_branch`'s guard would then never record it,
    // leaving the card naming no branch at all right after landing one.
    let after = f
        .manager
        .get_issue(&project.id, issue.number)
        .await
        .expect("issue");
    assert_eq!(
        after.branch.as_deref(),
        Some(baybo_project::worktree::branch_name(issue.number, &issue.title).as_str()),
        "the card must still name the branch it landed"
    );

    let landed = f
        .store
        .list_events(&issue.id)
        .await
        .expect("timeline")
        .into_iter()
        .find_map(|e| match e.body {
            IssueEventBody::BranchMerged { into, commits, .. } => Some((into, commits)),
            _ => None,
        })
        .expect("the timeline records the landing");
    assert_eq!(landed, (into, commits));
}

/// Done is the one column the board cannot start a run in — a comment there
/// wakes nobody, no status change triggers one, and the retry button refuses
/// outright — so the only merge a Done card can get is one made in the same
/// turn that closed it. That turn runs *after* reclamation has taken the
/// worktree, which is why this path is worth pinning: everything the merge
/// needs has to survive the checkout being gone.
#[tokio::test]
async fn a_card_closed_in_the_same_turn_can_still_land_its_branch() {
    let f = fixture().await;
    let (project, issue) = board_with_a_branch_to_land(&f, true).await;
    let root = baybo_project::worktree::worktree_root(&f.paths, &project.id, issue.number);

    f.manager
        .move_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueStatus::Done,
            &[issue.number],
        )
        .await
        .expect("close it");
    assert!(
        !root.exists(),
        "reclamation runs on the way into Done, so the checkout is gone by now"
    );

    let merged = f
        .manager
        .merge_issue_branch(&project.id, issue.number, IssueActor::User)
        .await
        .expect("merge");
    assert!(
        matches!(merged, baybo_project::worktree::Merged::Landed { .. }),
        "a reclaimed checkout must not stop the branch it left behind from landing: {merged:?}"
    );
}

/// Both halves are needed for this to bite, which is why they are in one
/// test: `branch_worked_on` reads the live checkout's own branch and only
/// falls back to re-deriving the name from the card's *current* title once
/// that checkout is gone. Close the card (reclamation takes the tree),
/// retitle it, and the fallback names a ref that never existed — while the
/// branch the card recorded is the one git actually knows.
#[tokio::test]
async fn a_retitled_card_lands_the_branch_it_recorded_not_one_named_after_the_new_title() {
    let f = fixture().await;
    let (project, issue) = board_with_a_branch_to_land(&f, true).await;
    let cut_as = baybo_project::worktree::branch_name(issue.number, &issue.title);

    // What a run's settle would have recorded, before anyone retitles.
    f.store
        .set_issue_branch(&issue.id, &cut_as)
        .await
        .expect("record the branch");
    f.manager
        .move_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueStatus::Done,
            &[issue.number],
        )
        .await
        .expect("close it, which reclaims the checkout");
    f.manager
        .update_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueUpdate {
                title: Some("something else entirely".to_owned()),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("retitle");

    let merged = f
        .manager
        .merge_issue_branch(&project.id, issue.number, IssueActor::User)
        .await
        .expect("merge");
    assert!(
        matches!(merged, baybo_project::worktree::Merged::Landed { .. }),
        "the recorded branch is the one git knows: {merged:?}"
    );
}

#[tokio::test]
async fn an_archived_board_does_not_merge() {
    let f = fixture().await;
    let (project, issue) = board_with_a_branch_to_land(&f, true).await;
    f.manager
        .set_project_archived(&project.id, true)
        .await
        .expect("archive");
    assert!(
        matches!(
            f.manager
                .merge_issue_branch(&project.id, issue.number, IssueActor::User)
                .await,
            Err(ProjectError::Archived(_))
        ),
        "an archived board is read-only, merging included"
    );
}

fn new_member(name: &str) -> baybo_project::NewTeamMember {
    baybo_project::NewTeamMember {
        name: name.to_owned(),
        role: "Writes the tests nobody else wants to.".to_owned(),
        framework: None,
        llm: baybo_model::LlmPin::unpinned(),
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
        .hire(&p.id, new_member("test-engineer"), Some(lead.clone()))
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
        .hire(&p.id, new_member("qa"), None)
        .await
        .expect("hire");
    assert_eq!(first.team.as_ref().map(|t| t.handle.as_str()), Some("qa"));

    let second = f
        .manager
        .hire(&p.id, new_member("qa"), None)
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
        .hire(&p.id, new_member("qa"), None)
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
                ..new_member("roleless")
            },
            "a missing role",
        ),
    ] {
        let refused = f.manager.hire(&p.id, member, None).await.expect_err(why);
        assert!(matches!(refused, ProjectError::Invalid { .. }), "{why}");
    }

    for n in 1..baybo_project::MAX_TEAM_AGENTS {
        f.manager
            .hire(&p.id, new_member(&format!("dev-{n}")), None)
            .await
            .unwrap_or_else(|e| panic!("hire {n}: {e}"));
    }
    let refused = f
        .manager
        .hire(&p.id, new_member("one-too-many"), None)
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
        .hire(&p.id, new_member("dev"), None)
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
        .hire(&theirs.id, new_member("dev"), None)
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
                daily_budget_tokens: None,
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
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
        .update_issue(&p.id, 1, IssueActor::User, IssueUpdate::default(), None)
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
            None,
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
            None,
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
            None,
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
            llm: baybo_model::LlmPin::unpinned(),
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
            None,
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

/// An agent that refuses itself leaves nothing on the card.
///
/// A reviewer bouncing a card back — comment, reassign, move Review → In
/// Progress — does all three from inside its own live run, so the move's
/// implied run is refused by the very run asking for it. Nothing is lost:
/// settling that run enqueues the follow-up that does the work. Recording
/// it told the operator "did not start a run — run #N still has this card"
/// about the run that asked, which is the one refusal nobody can act on.
#[tokio::test]
async fn an_agent_that_takes_its_own_cards_slot_records_nothing() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let reviewer = seed_agent(&f, &p.id, "reviewer", AgentFramework::Baybo).await;
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(reviewer.clone()),
                ..new_issue("under review")
            },
        )
        .await
        .expect("issue");
    let run = f.dispatched.lock()[0].clone();
    assert!(
        f.manager
            .start_run(&run, &SessionId::from("sess-review"))
            .await
            .expect("claim"),
        "the reviewer's run is live before it touches the card"
    );
    f.manager
        .move_issue(
            &p.id,
            1,
            IssueActor::Agent(reviewer.clone()),
            IssueStatus::Review,
            &[1],
        )
        .await
        .expect("park it in review");

    // Exactly what the reviewer's tool calls do: hand the card over, then
    // put it back in the working column. Only the move implies a run.
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::Agent(reviewer.clone()),
            IssueUpdate {
                assignee: Some(Some(dev.clone())),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("hand it over");
    f.manager
        .move_issue(
            &p.id,
            1,
            IssueActor::Agent(reviewer.clone()),
            IssueStatus::InProgress,
            &[1],
        )
        .await
        .expect("bounce it back");

    let timeline = f.manager.timeline(&p.id, 1).await.expect("timeline");
    assert!(
        timeline.iter().any(
            |e| matches!(&e.body, IssueEventBody::Assigned { to, .. } if to.as_ref() == Some(&dev))
        ),
        "the handover itself still stands"
    );
    assert!(
        !timeline
            .iter()
            .any(|e| matches!(e.body, IssueEventBody::RunRefused { .. })),
        "but the run its own live run refused is not a refusal the card reports"
    );
}

/// A swallowed handover says so on the card.
///
/// The refusal itself is the dedupe guard working, but the write that
/// implied the run has already committed: the card names @dev-2 and the
/// timeline carries the `Assigned` entry, while nothing started. Before
/// this entry existed the only trace was a log line, so the board asserted
/// a handover that never happened and `RunTrigger::Assigned` — which exists
/// precisely to stop that — was a no-op in the one case it is for.
#[tokio::test]
async fn a_run_the_cards_slot_refused_is_recorded_on_the_card() {
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
                ..new_issue("already running")
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
                assignee: Some(Some(other.clone())),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("reassign");

    let timeline = f.manager.timeline(&p.id, 1).await.expect("timeline");

    // The handover itself stood — that is what makes the silence a defect
    // rather than a refused write.
    assert!(
        timeline
            .iter()
            .any(|e| matches!(&e.body, IssueEventBody::Assigned { to, .. } if to.as_ref() == Some(&other))),
        "the reassignment was recorded"
    );

    let refused = timeline
        .iter()
        .find(|e| matches!(e.body, IssueEventBody::RunRefused { .. }))
        .expect("the card says the run it implied was not started");
    assert_eq!(
        refused.actor,
        IssueActor::System,
        "the board refused it, not the operator who asked"
    );
    let IssueEventBody::RunRefused { trigger, attempt } = &refused.body else {
        unreachable!("matched above")
    };
    assert_eq!(
        *trigger,
        RunTrigger::Assigned,
        "it names the run not started"
    );
    assert_eq!(
        *attempt,
        Some(1),
        "and names the run holding the slot, which is the half you can act on"
    );

    // Still one run: the entry is a record, not a second attempt.
    assert_eq!(f.manager.list_runs(&p.id, 1).await.expect("runs").len(), 1);
    assert_eq!(f.dispatched.lock().len(), 1);
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
async fn an_agent_may_not_take_back_a_cancel_the_operator_set() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let card = f
        .manager
        .create_issue(&p.id, IssueActor::User, new_issue("not doing this"))
        .await
        .expect("card")
        .into_issue();

    let cancelled = |cancelled: bool| IssueUpdate {
        cancelled: Some(cancelled),
        ..Default::default()
    };

    f.manager
        .update_issue(&p.id, card.number, IssueActor::User, cancelled(true), None)
        .await
        .expect("the operator calls it off");

    let refused = f
        .manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::Agent(dev.clone()),
            cancelled(false),
            None,
        )
        .await
        .expect_err("an agent reopened a card the operator cancelled");
    assert!(matches!(refused, ProjectError::Invalid { .. }), "{refused}");
    assert!(
        f.manager
            .get_issue(&p.id, card.number)
            .await
            .expect("issue")
            .cancelled_at
            .is_some(),
        "and the stop is still standing"
    );

    // The operator's own reopen is untouched — the gate reads the actor, not
    // the field.
    f.manager
        .update_issue(&p.id, card.number, IssueActor::User, cancelled(false), None)
        .await
        .expect("the operator reopens their own card");

    // A stop the board set is the board's to take back, which is the half of
    // this that has to keep working: nothing else clears a card the lead
    // called off in a run somebody has since answered.
    f.manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::Agent(dev.clone()),
            cancelled(true),
            None,
        )
        .await
        .expect("an agent calls it off");
    f.manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::Agent(dev),
            cancelled(false),
            None,
        )
        .await
        .expect("and takes its own cancel back");

    assert_eq!(
        f.manager
            .timeline(&p.id, card.number)
            .await
            .expect("timeline")
            .iter()
            .filter(|e| matches!(e.body, IssueEventBody::Uncancelled))
            .count(),
        2,
        "every reversal is on the record — reading whose stop is standing \
         depends on the timeline carrying both directions"
    );
}

#[tokio::test]
async fn a_persons_comment_takes_back_the_cancel_and_the_card_goes_again() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let card = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("the work")
            },
        )
        .await
        .expect("card")
        .into_issue();
    let started = f.dispatched.lock()[0].id.clone();
    f.store_settle(&started, RunStatus::Done).await;
    f.manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("the operator calls it off");
    f.dispatched.lock().clear();

    f.manager
        .comment(&p.id, card.number, IssueActor::User, "actually, do it", &[])
        .await
        .expect("say something on it");

    assert!(
        f.manager
            .get_issue(&p.id, card.number)
            .await
            .expect("issue")
            .cancelled_at
            .is_none(),
        "commenting on a card you called off is asking for it back"
    );
    let timeline = f
        .manager
        .timeline(&p.id, card.number)
        .await
        .expect("timeline");
    let marks: Vec<&IssueEventRow> = timeline
        .iter()
        .filter(|e| {
            matches!(
                e.body,
                IssueEventBody::Comment { .. } | IssueEventBody::Uncancelled
            )
        })
        .collect();
    assert!(
        matches!(marks[marks.len() - 2].body, IssueEventBody::Comment { .. })
            && matches!(marks[marks.len() - 1].body, IssueEventBody::Uncancelled),
        "the comment is the reason and reads before the reversal it caused"
    );
    assert_eq!(
        asks(&f),
        vec![RunTrigger::Comment],
        "and the card is live work again, woken by the comment that revived it"
    );
}

#[tokio::test]
async fn an_agents_comment_does_not_take_back_a_cancel() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let card = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("the work")
            },
        )
        .await
        .expect("card")
        .into_issue();
    let started = f.dispatched.lock()[0].id.clone();
    f.store_settle(&started, RunStatus::Done).await;
    f.manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("the operator calls it off");
    f.dispatched.lock().clear();

    // The side door `an_agent_may_not_take_back_a_cancel_the_operator_set`
    // closes on the front: saying a word must not do what asking outright is
    // refused for.
    f.manager
        .comment(
            &p.id,
            card.number,
            IssueActor::Agent(dev),
            "I still think we should",
            &[],
        )
        .await
        .expect("an agent may still say so");

    assert!(
        f.manager
            .get_issue(&p.id, card.number)
            .await
            .expect("issue")
            .cancelled_at
            .is_some(),
        "an agent talked a card out of the operator's cancel"
    );
    assert!(
        !f.manager
            .timeline(&p.id, card.number)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(e.body, IssueEventBody::Uncancelled)),
    );
    assert!(f.dispatched.lock().is_empty(), "and nothing runs");
}

#[tokio::test]
async fn a_revived_card_in_backlog_comes_back_without_starting_anything() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let card = park_card(
        &f,
        &p.id,
        "someday, maybe",
        IssueActor::User,
        Some(dev.clone()),
    )
    .await;
    f.manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("call it off");
    f.dispatched.lock().clear();

    f.manager
        .comment(&p.id, card.number, IssueActor::User, "keep this", &[])
        .await
        .expect("say something on it");

    assert!(
        f.manager
            .get_issue(&p.id, card.number)
            .await
            .expect("issue")
            .cancelled_at
            .is_none(),
        "it comes back"
    );
    assert_eq!(
        column_of(&f, &p.id, card.number).await,
        IssueStatus::Backlog,
        "into the column it was called off in"
    );
    tick(&f, &p.id).await;
    assert!(
        f.dispatched.lock().is_empty(),
        "and Backlog the operator parked still starts nothing: {:?}",
        asks(&f)
    );
}

#[tokio::test]
async fn an_agents_comment_does_not_revive_a_card_another_agent_called_off() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let card = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("the work")
            },
        )
        .await
        .expect("card")
        .into_issue();
    let started = f.dispatched.lock()[0].id.clone();
    f.store_settle(&started, RunStatus::Done).await;

    // An agent's cancel, which `update_issue` lets another agent reverse
    // outright — so this is the only gate standing between "say a word on the
    // card" and the board talking itself back into abandoned work.
    f.manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::Agent(lead),
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("the lead calls it off");
    f.dispatched.lock().clear();

    f.manager
        .comment(
            &p.id,
            card.number,
            IssueActor::Agent(dev),
            "I could still take this",
            &[],
        )
        .await
        .expect("an agent may still say so");

    assert!(
        f.manager
            .get_issue(&p.id, card.number)
            .await
            .expect("issue")
            .cancelled_at
            .is_some(),
        "an agent commented a cancelled card back into live work"
    );
    assert!(f.dispatched.lock().is_empty(), "and nothing runs");
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
            None,
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
            None,
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
            None,
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
        .comment(&p.id, 1, IssueActor::User, "@dev-1 pick this back up", &[])
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
            daily_budget_tokens: None,
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
async fn a_run_the_budget_held_says_held_in_the_answer_it_hands_back() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            daily_budget_tokens: None,
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
    // Free the card's dedupe slot, so the press below reaches the enqueue
    // rather than the refusal that sits above it.
    let first = f.manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
    f.store_settle(&first.id, RunStatus::Cancelled).await;
    f.dispatched.lock().clear();

    let started = f
        .manager
        .retry_run(&p.id, 1)
        .await
        .expect("the press was taken");
    // The row the enqueue answered with was written *before* the hold, so
    // handing it back unchanged told the caller the opposite of what the
    // ledger holds: this response said the run was on its way, and the
    // follow-up path logged "queued a follow-up" for a run the budget had
    // just stopped.
    assert_eq!(started.status, RunStatus::Held);
    let ledger = f.manager.list_runs(&p.id, 1).await.expect("runs");
    let recorded = ledger
        .iter()
        .find(|run| run.id == started.id)
        .expect("the row it claims to have started");
    assert_eq!(
        recorded.status,
        RunStatus::Held,
        "which is what the ledger says too"
    );
    assert!(
        f.dispatched.lock().is_empty(),
        "and nothing went out on a board with no room"
    );
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
            None,
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
            None,
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
            None,
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
            None,
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
    spend_since(project: &ProjectId, since: DateTime<Utc>) -> StoreResult<baybo_store::project::Spend>;
    attention() -> StoreResult<Vec<(ProjectId, AttentionCounts)>>;
    projects_for_sessions(sessions: &[SessionId]) -> StoreResult<Vec<(SessionId, ProjectId)>>;
    project_feed(project: &ProjectId, before: Option<DateTime<Utc>>, limit: usize)
        -> StoreResult<Vec<IssueEventRow>>;
    live_issue_by_source_key(project: &ProjectId, source_key: &str) -> StoreResult<Option<IssueRow>>;
    list_children(parent: &IssueId) -> StoreResult<Vec<IssueRow>>;
    hold_run(id: &IssueRunId) -> StoreResult<Option<baybo_store::project::IssueRunRow>>;
    release_run(id: &IssueRunId) -> StoreResult<Option<baybo_store::project::IssueRunRow>>;
    mark_issue_read(issue: &IssueId, at: DateTime<Utc>) -> StoreResult<bool>;
    mark_project_read(project: &ProjectId, at: DateTime<Utc>) -> StoreResult<usize>;
    card_signals(project: &ProjectId)
        -> StoreResult<std::collections::HashMap<IssueId, baybo_store::project::CardSignals>>;
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
    agent_opened_issues(project: &ProjectId) -> StoreResult<Vec<i64>>;
    events_since(issue: &IssueId, since: DateTime<Utc>) -> StoreResult<Vec<IssueEventRow>>;
    set_issue_branch(id: &IssueId, branch: &str) -> StoreResult<bool>;
    list_runs(issue: &IssueId) -> StoreResult<Vec<IssueRunRow>>;
    run_spend(issue: &IssueId) -> StoreResult<Vec<baybo_store::project::RunSpend>>;
    settled_run_facts(runs: &[baybo_model::IssueRunId])
        -> StoreResult<Vec<baybo_store::project::SettledRunFacts>>;
    board_activity(since: DateTime<Utc>)
        -> StoreResult<Vec<(ProjectId, baybo_store::project::BoardActivity)>>;
    active_runs(project: &ProjectId) -> StoreResult<Vec<IssueRunRow>>;
    drain_marks(project: &ProjectId) -> StoreResult<baybo_store::project::DrainMarks>;
    get_run(id: &IssueRunId) -> StoreResult<Option<IssueRunRow>>;
    claim_run(id: &IssueRunId, session: &SessionId) -> StoreResult<bool>;
    settle_run(id: &IssueRunId, status: RunStatus, error: Option<&str>) -> StoreResult<bool>;
    requeue_unsettled() -> StoreResult<Vec<IssueRunRow>>;
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
        Arc::clone(&f.blobs),
        f.paths.clone(),
        Arc::new(baybo_project::NoopProjectEvents),
        {
            let seen = Arc::clone(&dispatched);
            Arc::new(move |run| seen.lock().push(run))
        },
        baybo_project::no_stopper(),
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
            None,
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
            &[],
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
            baybo_store::project::IssueEventBody::Comment { text, .. } if text == "check the reconnect path"
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
        .comment(&project.id, issue.number, IssueActor::User, "   \n  ", &[])
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
        .comment(
            &project.id,
            parked.number,
            IssueActor::User,
            "some day",
            &[],
        )
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
        .comment(
            &project.id,
            live.number,
            IssueActor::User,
            "have a look",
            &[],
        )
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
            .comment(&project.id, issue.number, IssueActor::User, text, &[])
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
            None,
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
            daily_budget_tokens: None,
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

/// The sweep that settles a hold on a dead card used to sit *below* both of
/// the gates that stop a board — `release_holds` returns early on an
/// exhausted budget, and `promotions` returns early on `parallelism == 0`.
/// So the two deliberate ways to pause a board also paused the only thing
/// that could clear a hold the operator had already cancelled, and the
/// board's badge stayed lit on a row nothing would ever start.
#[tokio::test]
async fn a_hold_on_a_cancelled_card_is_called_off_even_on_a_stopped_board() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            daily_budget_tokens: None,
            ..new_project("Paused and skint")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    let issue = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("called off after it was held")
            },
        )
        .await
        .expect("create")
        .into_issue();

    assert_eq!(
        f.manager
            .list_runs(&project.id, issue.number)
            .await
            .expect("runs")[0]
            .status,
        RunStatus::Held
    );

    f.manager
        .update_issue(
            &project.id,
            issue.number,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("cancel the card");

    // Both gates shut: the budget is spent, and the board starts nothing on
    // its own.
    f.manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::ZERO),
                daily_budget_tokens: None,
                max_parallel_issue_runs: 0,
                agents_may_merge: false,
            },
        )
        .await
        .expect("stop the board");

    f.manager.drive(&project.id).await;

    let runs = f
        .manager
        .list_runs(&project.id, issue.number)
        .await
        .expect("runs");
    assert_eq!(
        runs[0].status,
        RunStatus::Cancelled,
        "a hold nothing will ever start is not work waiting for a slot"
    );
}

#[tokio::test]
async fn a_raised_budget_releases_what_it_was_holding() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            daily_budget_tokens: None,
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
                daily_budget_tokens: None,
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
            },
        )
        .await
        .expect("raise the ceiling");

    let dispatched = f.dispatched.lock().clone();
    assert_eq!(
        dispatched.len(),
        1,
        "the held run started once there was room"
    );
    // The released row, not the one `held_runs` read a moment earlier.
    // `IssueRunWaiter::enqueued` documents its copy as reading `Queued` for
    // the whole turn, and it can only do that if what it was handed says so.
    assert_eq!(
        dispatched[0].status,
        RunStatus::Queued,
        "and what went out is the row the release wrote"
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
            daily_budget_tokens: None,
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
                daily_budget_tokens: None,
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
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
            daily_budget_tokens: None,
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
                daily_budget_tokens: None,
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
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
                daily_budget_tokens: None,
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
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
                daily_budget_tokens: None,
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
                    None,
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
                    daily_budget_tokens: None,
                    max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                    agents_may_merge: false,
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
            daily_budget_tokens: None,
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
async fn a_board_over_its_token_ceiling_records_the_work_it_is_not_doing() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: None,
            daily_budget_tokens: Some(0),
            ..new_project("Token plan")
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
                ..new_issue("work there are no tokens for")
            },
        )
        .await
        .expect("create")
        .into_issue();

    assert!(
        f.dispatched.lock().is_empty(),
        "nothing was started against an exhausted token ceiling"
    );
    let runs = f.manager.list_runs(&project.id, 1).await.expect("runs");
    assert_eq!(runs[0].status, RunStatus::Held, "but the run was recorded");

    let timeline = f.manager.timeline(&project.id, 1).await.expect("timeline");
    let held = timeline
        .iter()
        .find(|e| {
            matches!(
                e.body,
                baybo_store::project::IssueEventBody::TokenBudgetExhausted { .. }
            )
        })
        .expect("the timeline names the ceiling that actually held it");
    assert!(
        matches!(
            held.body,
            baybo_store::project::IssueEventBody::TokenBudgetExhausted {
                spent_tokens: 0,
                limit_tokens: 0
            }
        ),
        "{:?}",
        held.body
    );
    assert_eq!(held.actor, IssueActor::System);
    assert!(
        !timeline.iter().any(|e| matches!(
            e.body,
            baybo_store::project::IssueEventBody::BudgetExhausted { .. }
        )),
        "and does not also claim a money ceiling this board never set"
    );
}

#[tokio::test]
async fn raising_the_ceiling_that_was_not_the_reason_releases_nothing() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::from_micros(5_000_000)),
            daily_budget_tokens: Some(0),
            ..new_project("Mixed")
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
                ..new_issue("held on tokens, not on money")
            },
        )
        .await
        .expect("create")
        .into_issue();
    assert!(f.dispatched.lock().is_empty(), "the token ceiling holds it");

    f.manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(500_000_000)),
                daily_budget_tokens: Some(0),
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
            },
        )
        .await
        .expect("raise the money ceiling");
    assert!(
        f.dispatched.lock().is_empty(),
        "money was never what was holding this run"
    );
    assert_eq!(
        f.manager.list_runs(&project.id, 1).await.expect("runs")[0].status,
        RunStatus::Held
    );

    f.manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(500_000_000)),
                daily_budget_tokens: Some(1_000_000),
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
            },
        )
        .await
        .expect("raise the one that was");

    assert_eq!(f.dispatched.lock().len(), 1, "and now it runs");
    assert!(
        f.manager
            .timeline(&project.id, 1)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(
                e.body,
                baybo_store::project::IssueEventBody::TokenBudgetRestored { .. }
            )),
        "the release is reported in the same unit the hold was"
    );
}

#[tokio::test]
async fn a_board_held_on_money_names_money_even_with_a_token_ceiling_set() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(NewProject {
            daily_budget: Some(baybo_model::MicroUsd::ZERO),
            daily_budget_tokens: Some(1_000_000),
            ..new_project("Both")
        })
        .await
        .expect("p");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;
    let issue = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev),
                ..new_issue("held on money, not on tokens")
            },
        )
        .await
        .expect("create")
        .into_issue();
    assert!(f.dispatched.lock().is_empty());

    let timeline = f.manager.timeline(&project.id, 1).await.expect("timeline");
    assert!(
        timeline.iter().any(|e| matches!(
            e.body,
            baybo_store::project::IssueEventBody::BudgetExhausted {
                spent_micros: 0,
                limit_micros: 0
            }
        )),
        "the card names the ceiling that actually stopped it: {:?}",
        timeline.iter().map(|e| e.body.kind()).collect::<Vec<_>>()
    );
    assert!(
        !timeline.iter().any(|e| matches!(
            e.body,
            baybo_store::project::IssueEventBody::TokenBudgetExhausted { .. }
        )),
        "and never claims a token ceiling with 100% of itself left was exhausted"
    );

    let refused = f
        .manager
        .retry_run(&project.id, issue.number)
        .await
        .expect_err("still held");
    let message = refused.to_string();
    assert!(
        message.contains("daily budget") && !message.contains("token"),
        "the operator is pointed at the ceiling raising which would help: {message}"
    );
}

#[tokio::test]
async fn a_negative_token_ceiling_is_refused_on_the_way_in_and_on_the_way_back() {
    let f = fixture().await;
    let refused = f
        .manager
        .create_project(NewProject {
            daily_budget_tokens: Some(-1),
            ..new_project("Impossible")
        })
        .await
        .expect_err("a board cannot open owing tokens");
    assert!(
        matches!(&refused, ProjectError::Invalid { field, .. } if *field == "daily_budget_tokens"),
        "and the form is told which field to point at: {refused:?}"
    );

    let project = f
        .manager
        .create_project(new_project("Fine"))
        .await
        .expect("p");
    let refused = f
        .manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: String::new(),
                daily_budget: None,
                daily_budget_tokens: Some(-1),
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
            },
        )
        .await
        .expect_err("nor edited into one");
    assert!(
        matches!(&refused, ProjectError::Invalid { field, .. } if *field == "daily_budget_tokens"),
        "{refused:?}"
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
            None,
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
            None,
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
async fn a_parent_the_operator_parked_is_not_woken_by_its_last_step() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Barrier"))
        .await
        .expect("p");
    let lead = f.manager.team(&p.id).await.expect("team")[0].id.clone();
    // Staffed and live, so the only thing holding the barrier shut is the
    // column the operator dropped it into.
    let parent = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(lead),
                status: IssueStatus::Backlog,
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
    f.dispatched.lock().clear();

    f.manager
        .move_issue(&p.id, 2, IssueActor::User, IssueStatus::Done, &[2])
        .await
        .expect("close the last step");
    assert!(
        f.dispatched.lock().is_empty(),
        "a parked parent was woken by its stage emptying"
    );
    assert!(
        f.manager
            .timeline(&p.id, parent.number)
            .await
            .expect("timeline")
            .iter()
            .any(|e| matches!(e.body, IssueEventBody::StageCompleted { stage: 0 })),
        "the stage still opened on the record, for whoever un-parks it"
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            daily_budget_tokens: None,
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
    // A board stopped by its own ceiling is a standing condition, not news:
    // it is reported in the board's header, never in the rail's mark.
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty(),
        "an over-budget board is not, on its own, something waiting on you"
    );
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].status,
        RunStatus::Held
    );

    f.manager
        .update_project(
            &p.id,
            ProjectUpdate {
                name: p.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::from_micros(5_000_000)),
                daily_budget_tokens: None,
                max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                agents_may_merge: false,
            },
        )
        .await
        .expect("raise");
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].status,
        RunStatus::Queued,
        "the hold was released"
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
            None,
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
            None,
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
                ..new_issue("broken")
            },
        )
        .await
        .expect("create")
        .into_issue();
    // A failure nobody has looked at, because that is a signal the rail still
    // carries. A hold used to stand in for one here and no longer can: a
    // board stopped by its own ceiling is a standing condition, reported in
    // the board's header rather than in the rail's mark.
    let run = f.manager.list_runs(&p.id, 1).await.expect("runs")[0]
        .id
        .clone();
    f.store_settle(&run, RunStatus::Failed).await;
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
        .comment(&p.id, 1, IssueActor::User, "any progress?", &[])
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
            &[],
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

    f.manager
        .mark_issue_read(&p.id, 1)
        .await
        .expect("mark read");
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

    // No read stamp anywhere in this test on purpose: a card nobody has
    // opened has no cursor, so every event on it is a candidate. What keeps
    // the count at zero is the predicate, not a cursor placed ahead of the
    // traffic.
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

/// The whole point of moving the cursor onto the card: reading the question
/// asked on one card must not silence the one asked on another. The board
/// cursor this replaced could only clear both or neither.
#[tokio::test]
async fn reading_one_card_leaves_every_other_cards_count_alone() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Two questions"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    for title in ["first", "second"] {
        f.manager
            .create_issue(
                &p.id,
                IssueActor::User,
                NewIssueRequest {
                    assignee: Some(dev.clone()),
                    status: IssueStatus::Todo,
                    ..new_issue(title)
                },
            )
            .await
            .expect("create");
    }
    for number in [1, 2] {
        f.manager
            .comment(
                &p.id,
                number,
                IssueActor::Agent(dev.clone()),
                "which way?",
                &[],
            )
            .await
            .expect("comment");
    }

    let signals = f.manager.board_cards(&p.id).await.expect("board");
    let unread = |number: i64| {
        let row = signals
            .rows
            .iter()
            .find(|row| row.number == number)
            .expect("row");
        signals.signals(&row.id).unread
    };
    assert_eq!(unread(1), 1);
    assert_eq!(unread(2), 1);
    assert_eq!(
        f.manager.attention(&[]).await.expect("attention")[0]
            .1
            .unread,
        2,
        "the board's count is the sum of its cards'"
    );

    f.manager
        .mark_issue_read(&p.id, 1)
        .await
        .expect("mark read");

    let signals = f.manager.board_cards(&p.id).await.expect("board");
    let unread = |number: i64| {
        let row = signals
            .rows
            .iter()
            .find(|row| row.number == number)
            .expect("row");
        signals.signals(&row.id).unread
    };
    assert_eq!(unread(1), 0);
    assert_eq!(unread(2), 1, "#2 was never opened");
    assert_eq!(
        f.manager.attention(&[]).await.expect("attention")[0]
            .1
            .unread,
        1
    );
}

/// One press clears the board the operator is looking at, and stops at its
/// edge. It is still the per-card cursor doing the work — every card gets
/// its own stamp — so the board next door keeps everything it was holding.
#[tokio::test]
async fn one_press_reads_this_board_and_no_other() {
    let f = fixture().await;
    let here = f
        .manager
        .create_project(new_project("Here"))
        .await
        .expect("here");
    let there = f
        .manager
        .create_project(new_project("There"))
        .await
        .expect("there");
    let dev = seed_agent(&f, &here.id, "dev-1", AgentFramework::Baybo).await;
    let neighbour = seed_agent(&f, &there.id, "dev-2", AgentFramework::Baybo).await;

    for (project, agent) in [(&here.id, &dev), (&here.id, &dev), (&there.id, &neighbour)] {
        let issue = f
            .manager
            .create_issue(project, IssueActor::User, new_issue("work"))
            .await
            .expect("create")
            .into_issue();
        f.manager
            .comment(
                project,
                issue.number,
                IssueActor::Agent(agent.clone()),
                "which way?",
                &[],
            )
            .await
            .expect("comment");
    }

    let unread_on = |project: baybo_model::ProjectId| {
        let manager = &f.manager;
        async move {
            let cards = manager.board_cards(&project).await.expect("board");
            cards
                .rows
                .iter()
                .map(|row| cards.signals(&row.id).unread)
                .sum::<usize>()
        }
    };
    assert_eq!(unread_on(here.id.clone()).await, 2);
    assert_eq!(unread_on(there.id.clone()).await, 1);

    f.manager
        .mark_project_read(&here.id)
        .await
        .expect("mark the board read");
    assert_eq!(unread_on(here.id.clone()).await, 0);
    assert_eq!(
        unread_on(there.id.clone()).await,
        1,
        "the board next door was not read"
    );
    // The rail and the cards are two readings of one predicate, so the press
    // has to take both out together — a dot left over a board on which every
    // card reads zero is the drift those constants exist to prevent.
    let lit = f.manager.attention(&[]).await.expect("attention");
    assert_eq!(
        lit.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        vec![there.id.clone()]
    );

    // The stamp says "seen up to here", not "stop counting": what an agent
    // says next is news again, on the very card that was just cleared.
    f.manager
        .comment(&here.id, 1, IssueActor::Agent(dev), "one more thing", &[])
        .await
        .expect("comment");
    assert_eq!(unread_on(here.id.clone()).await, 1);
}

/// Noting that something was seen is not an addition to the board, so it is
/// one of the writes a shelved board still takes — the same exemption
/// `mark_issue_read` has.
#[tokio::test]
async fn a_shelved_board_can_still_be_read() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Shelved"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let issue = f
        .manager
        .create_issue(&p.id, IssueActor::User, new_issue("work"))
        .await
        .expect("create")
        .into_issue();
    f.manager
        .comment(
            &p.id,
            issue.number,
            IssueActor::Agent(dev),
            "left you a note",
            &[],
        )
        .await
        .expect("comment");
    f.manager
        .set_project_archived(&p.id, true)
        .await
        .expect("archive");

    f.manager
        .mark_project_read(&p.id)
        .await
        .expect("a shelved board still takes the stamp");
    let cards = f.manager.board_cards(&p.id).await.expect("board");
    assert_eq!(cards.signals(&cards.rows[0].id).unread, 0);
}

/// The operator's own drag into Review used to make their own board
/// announce itself back at them: the `moved` arm carried no actor filter
/// while the `comment` arm did.
#[tokio::test]
async fn your_own_drag_into_review_is_not_news_to_you() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Tidying up"))
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
                ..new_issue("mine to file")
            },
        )
        .await
        .expect("create");

    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Review, &[1])
        .await
        .expect("file it myself");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .iter()
            .all(|(_, c)| c.unread == 0),
        "you filed it; nobody is handing it back to you"
    );

    // Back out of Review first: `move_issue` to the column a card is
    // already in writes no `Moved` row, so re-filing it as the agent would
    // otherwise test nothing.
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Todo, &[1])
        .await
        .expect("take it back");
    f.manager
        .move_issue(&p.id, 1, IssueActor::Agent(dev), IssueStatus::Review, &[1])
        .await
        .expect("handed back");
    assert_eq!(
        f.manager.attention(&[]).await.expect("attention")[0]
            .1
            .unread,
        1,
        "an agent handing it back is"
    );
}

/// A rail dot reading "1 failed" always has a card admitting to it: the
/// count is the card's own predicate AND "you have not looked since". It can
/// therefore go quiet while the card still wears its badge — and never the
/// reverse, which is the reading that would send an operator to a board
/// where every card says zero.
#[tokio::test]
async fn a_failed_run_marks_its_card_and_the_board_together() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Failures"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let issue = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                status: IssueStatus::InProgress,
                ..new_issue("will fail")
            },
        )
        .await
        .expect("create")
        .into_issue();

    let board = f.manager.board_cards(&p.id).await.expect("board");
    assert!(!board.signals(&issue.id).last_run_failed);

    let run = f
        .manager
        .list_runs(&p.id, issue.number)
        .await
        .expect("runs")[0]
        .id
        .clone();
    f.store_settle(&run, RunStatus::Failed).await;

    let board = f.manager.board_cards(&p.id).await.expect("board");
    assert!(board.signals(&issue.id).last_run_failed);
    assert_eq!(
        f.manager.attention(&[]).await.expect("attention")[0]
            .1
            .failed,
        1
    );

    // Reading is what puts the RAIL out: its mark is a pointer, and one that
    // survives being followed is noise. The card is still broken, and the
    // board still says so — a failure is discharged by acting, and only the
    // "come and look" is discharged by looking.
    f.manager
        .mark_issue_read(&p.id, issue.number)
        .await
        .expect("mark read");
    assert!(
        f.manager
            .attention(&[])
            .await
            .expect("attention")
            .is_empty(),
        "following the pointer puts it out"
    );
    let board = f.manager.board_cards(&p.id).await.expect("board");
    assert!(
        board.signals(&issue.id).last_run_failed,
        "the card is still broken, and the board still says so"
    );

    // Failing AGAIN is news again, off the same cursor — no second rule.
    let again = f
        .manager
        .retry_run(&p.id, issue.number)
        .await
        .expect("retry");
    let board = f.manager.board_cards(&p.id).await.expect("board");
    assert!(
        !board.signals(&issue.id).last_run_failed,
        "the newest run is no longer the failed one"
    );
    f.store_settle(&again.id, RunStatus::Failed).await;
    assert_eq!(
        f.manager.attention(&[]).await.expect("attention")[0]
            .1
            .failed,
        1,
        "a fresh failure relights it without being read again"
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
        .comment(
            &p.id,
            1,
            IssueActor::User,
            "@dev-1 could you take this?",
            &[],
        )
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
            &[],
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
        .comment(&p.id, 1, IssueActor::User, "@dev-2 what do you think?", &[])
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
        .comment(&p.id, 1, IssueActor::User, "@nobody-here please look", &[])
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
            .any(|e| matches!(&e.body, baybo_store::project::IssueEventBody::Comment { text, .. } if text.contains("nobody-here")))
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

    // No live turn to stop, session or no session: the row is settled where
    // it stands rather than handed to a stopper that would find nothing.
    f.manager.cancel_run(&p.id, 1).await.expect("cancel");
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

/// Records what the board asked to stop, and stops nothing: the executor
/// that would settle the row is not running in these tests.
struct RecordingStopper {
    stopped: Arc<parking_lot::Mutex<Vec<(SessionId, baybo_project::RunStopReason)>>>,
}

#[async_trait::async_trait]
impl baybo_project::IssueRunStopper for RecordingStopper {
    async fn stop_run(
        &self,
        session: &SessionId,
        reason: baybo_project::RunStopReason,
    ) -> Result<(), String> {
        self.stopped.lock().push((session.clone(), reason));
        Ok(())
    }
}

/// A live run is its executor's, even when the card it is on is finished.
///
/// `redrive_after_unblock` is the one door into `card_for` that does not
/// filter by status, and `verdict` used to answer `CallOff` on a finished
/// card before it looked at the row. So a single `update_issue` that lifted
/// a block *and* cancelled the card settled the row `Cancelled` underneath
/// the agent still working it — and stamped it "interrupted before it could
/// resume", which it was not. Two writes then raced to record one outcome.
#[tokio::test]
async fn unblocking_and_cancelling_at_once_leaves_a_live_run_to_its_executor() {
    let (f, p, _dev, run) = card_being_worked().await;
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(Some("waiting on a decision".into())),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("block it");

    // Both halves in one patch: the unblock is what reaches
    // `redrive_after_unblock`, the cancel is what makes the card stop
    // accepting runs. Either alone leaves the row alone.
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(None),
                cancelled: Some(true),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("unblock and cancel");

    let after = f.manager.list_runs(&p.id, 1).await.expect("runs")[0].clone();
    assert_eq!(after.id, run.id);
    assert_eq!(
        after.status,
        RunStatus::Running,
        "the board does not settle a row its executor owns"
    );
    assert!(
        after.settled_at.is_none(),
        "and leaves the settle to whoever is watching the turn"
    );
}

/// A board over its money ceiling stops the run that is spending it.
///
/// The enqueue gate only ever decided whether the *next* run starts, so a
/// ceiling bounded nothing: one run's spend is unbounded, and the observed
/// overshoot was 2x-55x the daily limit.
#[tokio::test]
async fn a_board_over_its_money_ceiling_stops_the_run_that_is_spending_it() {
    let (f, p, _dev, run) = card_being_worked().await;

    f.manager
        .update_project(
            &p.id,
            ProjectUpdate {
                name: p.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::ZERO),
                daily_budget_tokens: None,
                max_parallel_issue_runs: 3,
                agents_may_merge: false,
            },
        )
        .await
        .expect("skint");

    tick(&f, &p.id).await;

    assert_eq!(
        f.stopped.lock().as_slice(),
        [(
            SessionId::from("sess-1"),
            baybo_project::RunStopReason::BudgetExhausted
        )],
        "the live run is interrupted, not left to spend the rest of the day"
    );

    // The cancel is asynchronous, so the row is still `Running` on the next
    // pass. Telling it again would spam both the turn layer and the card.
    tick(&f, &p.id).await;
    assert_eq!(
        f.stopped.lock().len(),
        1,
        "a run already told to stop is not told again while it winds down"
    );
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].id,
        run.id,
        "and nothing here settles the row — its executor does that"
    );
}

/// A token ceiling is soft on purpose.
///
/// Tokens measure subscription plans, where the turn is paid for whether or
/// not it is allowed to finish, so throwing it away buys nothing. The board
/// still refuses to *start* more work — that half is unchanged.
#[tokio::test]
async fn a_spent_token_ceiling_lets_the_run_it_is_measuring_finish() {
    let (f, p, _dev, _run) = card_being_worked().await;

    f.manager
        .update_project(
            &p.id,
            ProjectUpdate {
                name: p.name.clone(),
                description: String::new(),
                daily_budget: None,
                daily_budget_tokens: Some(0),
                max_parallel_issue_runs: 3,
                agents_may_merge: false,
            },
        )
        .await
        .expect("out of tokens");

    tick(&f, &p.id).await;

    assert!(
        f.stopped.lock().is_empty(),
        "nothing in flight is stopped for a ceiling that costs no money"
    );
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

    // A queued run is settled where it stands; there is no session to stop.
    f.manager.cancel_run(&p.id, 1).await.expect("cancel");
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs")[0].status,
        RunStatus::Cancelled
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
        .comment(&p.id, 1, IssueActor::User, "start with the CSV path", &[])
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
        .comment(
            &p.id,
            1,
            IssueActor::User,
            "also handle the empty case",
            &[],
        )
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
            &[],
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
        .comment(
            &p.id,
            1,
            IssueActor::User,
            "also handle the empty case",
            &[],
        )
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
        .comment(&p.id, 1, IssueActor::User, "actually, stop", &[])
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
        agents: Arc::clone(&f.agents),
        blobs: Arc::clone(&f.blobs),
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

#[tokio::test]
async fn a_brief_names_who_said_each_thing_on_the_card() {
    let f = fixture().await;
    let p = f.manager.create_project(new_project("p")).await.expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let qa = seed_agent(&f, &p.id, "qa", AgentFramework::Baybo).await;
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
    for (actor, text) in [
        (IssueActor::User, "start with the CSV path"),
        (
            IssueActor::Agent(qa),
            "@dev-1 does it skip rows with no id?",
        ),
        (IssueActor::Agent(dev), "picking this up"),
    ] {
        f.manager
            .comment(&p.id, 1, actor, text, &[])
            .await
            .expect("comment");
    }

    let run = f.dispatched.lock()[0].clone();
    let (tx, _announced) = tokio::sync::mpsc::unbounded_channel();
    let (dispatch, mut prepared) = baybo_project::dispatch::build(baybo_project::DispatchConfig {
        store: Arc::clone(&f.store),
        agents: Arc::clone(&f.agents),
        blobs: Arc::clone(&f.blobs),
        events: Arc::new(RecordingEvents(tx)),
        paths: f.paths.clone(),
        user_id: "owner".to_owned(),
        channel: baybo_model::ChannelType::owner(),
    });
    dispatch(run);

    let event = tokio::time::timeout(std::time::Duration::from_secs(10), prepared.recv())
        .await
        .expect("the dispatcher prepares the run")
        .expect("the run reaches its executor");

    // Who said it is what decides whether an answer is owed and to whom.
    // Unattributed, the operator's instruction and a teammate's question
    // arrive as one voice — and the agent's own note reads as somebody
    // else telling it what it already did.
    for line in [
        "- the operator: start with the CSV path\n",
        "- @qa: @dev-1 does it skip rows with no id?\n",
        "- @dev-1: picking this up\n",
    ] {
        assert!(
            event.brief.contains(line),
            "{line:?} is missing from the brief:\n{}",
            event.brief
        );
    }
}

// ---------------------------------------------------------------------------
// The driver: what the board starts without being told to.
// ---------------------------------------------------------------------------

/// A board with `slots` of room and one agent to fill it with.
async fn driven_board(f: &Fixture, slots: usize) -> (ProjectRow, AgentProfileId) {
    let project = f
        .manager
        .create_project(NewProject {
            max_parallel_issue_runs: Some(slots),
            ..driven_project("Self-driving")
        })
        .await
        .expect("project");
    let dev = seed_agent(f, &project.id, "dev-1", AgentFramework::Baybo).await;
    (project, dev)
}

/// Open a card straight into Todo, staffed and ready.
async fn queue_card(
    f: &Fixture,
    project: &ProjectId,
    title: &str,
    assignee: Option<AgentProfileId>,
    priority: IssuePriority,
) -> IssueRow {
    f.manager
        .create_issue(
            project,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Todo,
                assignee,
                priority,
                ..new_issue(title)
            },
        )
        .await
        .expect("queue")
        .into_issue()
}

/// Open a card straight into Backlog, as whoever filed it.
async fn park_card(
    f: &Fixture,
    project: &ProjectId,
    title: &str,
    filed_by: IssueActor,
    assignee: Option<AgentProfileId>,
) -> IssueRow {
    f.manager
        .create_issue(
            project,
            filed_by,
            NewIssueRequest {
                status: IssueStatus::Backlog,
                assignee,
                ..new_issue(title)
            },
        )
        .await
        .expect("park")
        .into_issue()
}

async fn set_ceiling(f: &Fixture, project: &ProjectRow, slots: usize) {
    f.manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: project.description.clone(),
                daily_budget: project.daily_budget,
                daily_budget_tokens: None,
                max_parallel_issue_runs: slots,
                agents_may_merge: project.agents_may_merge,
            },
        )
        .await
        .expect("ceiling");
}

async fn set_merge(f: &Fixture, project: &ProjectRow, may_merge: bool) {
    f.manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: project.description.clone(),
                daily_budget: project.daily_budget,
                daily_budget_tokens: project.daily_budget_tokens,
                max_parallel_issue_runs: project.max_parallel_issue_runs,
                agents_may_merge: may_merge,
            },
        )
        .await
        .expect("merge rule");
}

/// Every run the board has handed out, by why it handed it out.
fn asks(f: &Fixture) -> Vec<RunTrigger> {
    f.dispatched.lock().iter().map(|run| run.trigger).collect()
}

async fn set_budget(f: &Fixture, project: &ProjectRow, budget: Option<baybo_model::MicroUsd>) {
    f.manager
        .update_project(
            &project.id,
            ProjectUpdate {
                name: project.name.clone(),
                description: project.description.clone(),
                daily_budget: budget,
                daily_budget_tokens: None,
                max_parallel_issue_runs: project.max_parallel_issue_runs,
                agents_may_merge: project.agents_may_merge,
            },
        )
        .await
        .expect("budget");
}

async fn lead_of(f: &Fixture, project: &ProjectId) -> AgentProfileId {
    f.manager
        .team(project)
        .await
        .expect("team")
        .into_iter()
        .find(|row| {
            row.team
                .as_ref()
                .is_some_and(|t| t.handle.as_str() == baybo_project::LEAD_HANDLE)
        })
        .expect("every board has a lead")
        .id
}

/// Give the board its turn.
///
/// In production this is the sweep's tick and nothing else; here it is
/// explicit so a test asserting the board did *nothing* cannot pass merely
/// because the board was never asked.
async fn tick(f: &Fixture, project: &ProjectId) -> usize {
    f.manager.drive(project).await
}

async fn column_of(f: &Fixture, project: &ProjectId, number: i64) -> IssueStatus {
    f.manager
        .get_issue(project, number)
        .await
        .expect("issue")
        .status
}

/// Open a staffed Todo card as a step of `parent`, at `stage`.
async fn queue_step(
    f: &Fixture,
    project: &ProjectId,
    title: &str,
    assignee: &AgentProfileId,
    priority: IssuePriority,
    parent: i64,
    stage: i64,
) -> IssueRow {
    f.manager
        .create_issue(
            project,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Todo,
                assignee: Some(assignee.clone()),
                priority,
                parent: Some(parent),
                stage,
                ..new_issue(title)
            },
        )
        .await
        .expect("step")
        .into_issue()
}

/// The incident, in the shape it actually had: a lead filed a plan under
/// one card — three steps at stage 1, a final acceptance step at stage 3 —
/// and the board started the stage-3 step before any stage-1 step had run,
/// because nothing on the starting path read `stage`. Its assignee
/// spent a run discovering there was nothing to accept yet and blocked the
/// card, and that prose block is what went on to wake the lead nine times.
///
/// `urgent` on the last step is not decoration: `promotion_order` sorts by
/// priority, so the step furthest from being startable was the first card
/// the board reached for.
#[tokio::test]
async fn a_later_stage_is_not_promoted_while_an_earlier_one_is_open() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 5).await;
    let plan = queue_card(
        &f,
        &p.id,
        "the plan",
        Some(dev.clone()),
        IssuePriority::None,
    )
    .await;

    for step in 1..=3 {
        queue_step(
            &f,
            &p.id,
            &format!("stage one, step {step}"),
            &dev,
            IssuePriority::High,
            plan.number,
            1,
        )
        .await;
    }
    let acceptance = queue_step(
        &f,
        &p.id,
        "final acceptance",
        &dev,
        IssuePriority::Urgent,
        plan.number,
        3,
    )
    .await;

    tick(&f, &p.id).await;

    assert_eq!(
        column_of(&f, &p.id, acceptance.number).await,
        IssueStatus::Todo,
        "a step whose stage the board has not reached stays where it is"
    );
    let started: Vec<i64> = f.dispatched.lock().iter().map(|run| run.number).collect();
    assert!(
        !started.contains(&acceptance.number),
        "and nothing was started on it: {started:?}"
    );
    assert!(
        started.len() >= 2,
        "while the stage the board *has* reached runs, urgent or not: {started:?}"
    );
}

/// The override the board must never take back: a person dragging a held
/// step into In Progress is overruling the plan on purpose, exactly as they
/// may overrule a block. The gate lives in `driver::is_waiting`, which only
/// the board's own two doors ask.
#[tokio::test]
async fn an_operator_starting_a_held_step_still_starts_it() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 5).await;
    let plan = queue_card(
        &f,
        &p.id,
        "the plan",
        Some(dev.clone()),
        IssuePriority::None,
    )
    .await;
    let first = queue_step(
        &f,
        &p.id,
        "stage one",
        &dev,
        IssuePriority::High,
        plan.number,
        1,
    )
    .await;
    let later = queue_step(
        &f,
        &p.id,
        "stage two",
        &dev,
        IssuePriority::High,
        plan.number,
        2,
    )
    .await;

    f.manager
        .move_issue(
            &p.id,
            later.number,
            IssueActor::User,
            IssueStatus::InProgress,
            &[later.number],
        )
        .await
        .expect("drag");

    let started: Vec<i64> = f.dispatched.lock().iter().map(|run| run.number).collect();
    assert!(
        started.contains(&later.number),
        "the operator's own move starts the card it names: {started:?}"
    );
    assert_eq!(
        column_of(&f, &p.id, first.number).await,
        IssueStatus::Todo,
        "and the stage it jumped is still open, which is the point of the override"
    );
}

/// "Cancel the step you are not doing" is how an operator opens a barrier,
/// and it has to work on the way *in* as well as on the way out —
/// `stages::is_finished` is the one definition both sides read.
#[tokio::test]
async fn a_cancelled_step_does_not_hold_the_next_stage_shut() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 5).await;
    let plan = queue_card(
        &f,
        &p.id,
        "the plan",
        Some(dev.clone()),
        IssuePriority::None,
    )
    .await;
    let dropped = queue_step(
        &f,
        &p.id,
        "stage one",
        &dev,
        IssuePriority::High,
        plan.number,
        1,
    )
    .await;
    let later = queue_step(
        &f,
        &p.id,
        "stage two",
        &dev,
        IssuePriority::High,
        plan.number,
        2,
    )
    .await;

    tick(&f, &p.id).await;
    assert_eq!(
        column_of(&f, &p.id, later.number).await,
        IssueStatus::Todo,
        "the control: stage two waits while stage one is open"
    );

    f.manager
        .update_issue(
            &p.id,
            dropped.number,
            IssueActor::User,
            IssueUpdate {
                cancelled: Some(true),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("cancel");
    tick(&f, &p.id).await;

    assert_eq!(
        column_of(&f, &p.id, later.number).await,
        IssueStatus::InProgress,
        "a step nobody is doing any more is not a step the next stage waits for"
    );
}

#[tokio::test]
async fn a_staffed_card_in_todo_starts_itself() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 2).await;

    queue_card(
        &f,
        &p.id,
        "wire the importer",
        Some(dev),
        IssuePriority::None,
    )
    .await;
    assert_eq!(
        column_of(&f, &p.id, 1).await,
        IssueStatus::Todo,
        "the write itself starts nothing — the board is driven on a tick"
    );

    assert_eq!(tick(&f, &p.id).await, 1);
    assert_eq!(
        column_of(&f, &p.id, 1).await,
        IssueStatus::InProgress,
        "Todo is a queue the board pulls from, so a ready card does not sit in it"
    );
    let dispatched = f.dispatched.lock().clone();
    assert_eq!(
        dispatched.len(),
        1,
        "and something is actually running on it"
    );
    assert_eq!(
        dispatched[0].trigger,
        RunTrigger::Promoted,
        "recorded as the board's doing, not as a drag nobody performed"
    );
    assert!(
        f.manager
            .timeline(&p.id, 1)
            .await
            .expect("timeline")
            .iter()
            .any(|e| e.actor == IssueActor::System
                && matches!(
                    e.body,
                    baybo_store::project::IssueEventBody::Moved {
                        to: IssueStatus::InProgress,
                        ..
                    }
                )),
        "and the card says the board moved it, so the column change is not unexplained"
    );
}

#[tokio::test]
async fn the_ceiling_is_what_stops_it() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 2).await;

    for title in ["first", "second", "third"] {
        queue_card(&f, &p.id, title, Some(dev.clone()), IssuePriority::None).await;
    }
    assert_eq!(tick(&f, &p.id).await, 2);

    assert_eq!(
        f.dispatched.lock().len(),
        2,
        "two slots, two runs — the third waits where the operator can see it"
    );
    assert_eq!(column_of(&f, &p.id, 3).await, IssueStatus::Todo);

    let first = f.dispatched.lock()[0].clone();
    f.manager
        .finish_run(
            &first,
            std::path::Path::new("/nonexistent/checkout"),
            first.created_at,
            baybo_project::RunOutcome {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
        )
        .await;
    assert_eq!(tick(&f, &p.id).await, 1);

    assert_eq!(
        column_of(&f, &p.id, 3).await,
        IssueStatus::InProgress,
        "and the slot a finished run gives back is filled without anybody asking"
    );
    assert_eq!(f.dispatched.lock().len(), 3);
}

#[tokio::test]
async fn a_board_told_to_start_nothing_starts_nothing() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 0).await;

    queue_card(
        &f,
        &p.id,
        "wire the importer",
        Some(dev),
        IssuePriority::None,
    )
    .await;
    assert_eq!(tick(&f, &p.id).await, 0);

    assert_eq!(column_of(&f, &p.id, 1).await, IssueStatus::Todo);
    assert!(
        f.dispatched.lock().is_empty(),
        "a ceiling of zero is the manual board, and it has to still exist"
    );
}

#[tokio::test]
async fn urgent_work_is_taken_first() {
    let f = fixture().await;
    // No room while the queue fills: the board cannot preempt a card it has
    // already started, so a priority rule only means anything about cards
    // that were both waiting when the slot appeared.
    let (p, dev) = driven_board(&f, 0).await;
    queue_card(&f, &p.id, "tidy up", Some(dev.clone()), IssuePriority::Low).await;
    queue_card(&f, &p.id, "prod is down", Some(dev), IssuePriority::Urgent).await;
    assert_eq!(tick(&f, &p.id).await, 0);
    assert!(f.dispatched.lock().is_empty());

    set_ceiling(&f, &p, 1).await;
    assert_eq!(tick(&f, &p.id).await, 1);

    assert_eq!(
        f.dispatched.lock()[0].number,
        2,
        "the one slot goes to the urgent card, not the one that was queued first"
    );
    assert_eq!(column_of(&f, &p.id, 1).await, IssueStatus::Todo);
}

#[tokio::test]
async fn the_board_does_not_start_work_somebody_stopped() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;

    // Blocked before it reaches Todo, because a card that arrives ready is
    // started before anybody could block it.
    let blocked = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                assignee: Some(dev.clone()),
                ..new_issue("blocked")
            },
        )
        .await
        .expect("create")
        .into_issue();
    f.manager
        .update_issue(
            &p.id,
            blocked.number,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(Some("waiting on the operator".into())),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("block");
    f.manager
        .move_issue(
            &p.id,
            blocked.number,
            IssueActor::User,
            IssueStatus::Todo,
            &[blocked.number],
        )
        .await
        .expect("to todo");
    assert_eq!(tick(&f, &p.id).await, 0);

    assert_eq!(
        column_of(&f, &p.id, blocked.number).await,
        IssueStatus::Todo,
        "a block is a person saying stop; the board does not overrule it"
    );
    assert!(f.dispatched.lock().is_empty());

    f.manager
        .update_issue(
            &p.id,
            blocked.number,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(None),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("unblock");
    assert_eq!(tick(&f, &p.id).await, 1);
    assert_eq!(
        column_of(&f, &p.id, blocked.number).await,
        IssueStatus::InProgress,
        "and unblocking it is enough to get it moving again"
    );
}

#[tokio::test]
async fn an_exhausted_budget_parks_the_driver_rather_than_filling_in_progress() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    f.manager
        .update_project(
            &p.id,
            ProjectUpdate {
                name: p.name.clone(),
                description: String::new(),
                daily_budget: Some(baybo_model::MicroUsd::ZERO),
                daily_budget_tokens: None,
                max_parallel_issue_runs: 3,
                agents_may_merge: false,
            },
        )
        .await
        .expect("skint");

    queue_card(
        &f,
        &p.id,
        "wire the importer",
        Some(dev),
        IssuePriority::None,
    )
    .await;
    assert_eq!(tick(&f, &p.id).await, 0);

    assert_eq!(
        column_of(&f, &p.id, 1).await,
        IssueStatus::Todo,
        "In Progress means an agent is working now, and a held run is not that"
    );
    assert!(f.dispatched.lock().is_empty());
}

#[tokio::test]
async fn an_unstaffed_card_in_todo_wakes_the_lead_once() {
    let f = fixture().await;
    let (p, _dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    let card = queue_card(
        &f,
        &p.id,
        "somebody should do this",
        None,
        IssuePriority::None,
    )
    .await;
    tick(&f, &p.id).await;

    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "the lead is woken to staff it");
    assert_eq!(asked[0].trigger, RunTrigger::Triage);
    assert_eq!(asked[0].agent_id, lead, "on a card nobody is assigned to");
    assert_eq!(
        column_of(&f, &p.id, card.number).await,
        IssueStatus::Todo,
        "and the card does not move: nothing is being worked on yet"
    );

    // The lead reads it and leaves it alone, which is a legitimate answer.
    let triage = asked[0].clone();
    f.manager
        .finish_run(
            &triage,
            std::path::Path::new("/nonexistent/checkout"),
            triage.created_at,
            baybo_project::RunOutcome {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
        )
        .await;
    tick(&f, &p.id).await;
    tick(&f, &p.id).await;

    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "and it is not asked the same question again on every later pass"
    );
}

#[tokio::test]
async fn the_lead_is_asked_about_the_backlog_the_board_filed_and_never_about_the_operators() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    let theirs = park_card(&f, &p.id, "someday, maybe", IssueActor::User, None).await;
    let ours = park_card(
        &f,
        &p.id,
        "spun out of the last run",
        IssueActor::Agent(dev.clone()),
        Some(dev.clone()),
    )
    .await;

    tick(&f, &p.id).await;
    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "one question, and only one card can be it");
    assert_eq!(asked[0].trigger, RunTrigger::Grooming);
    assert_eq!(
        asked[0].number, ours.number,
        "the board asks about the card it filed itself, not the one the operator parked"
    );
    assert_eq!(
        asked[0].agent_id, lead,
        "and the lead is who answers for it"
    );
    assert_eq!(
        column_of(&f, &p.id, ours.number).await,
        IssueStatus::Backlog,
        "asking is not moving: Backlog is left for the lead to empty"
    );
    assert_eq!(
        column_of(&f, &p.id, theirs.number).await,
        IssueStatus::Backlog,
        "and the operator's parked card is not touched at all"
    );

    // The lead looks and decides it is not ready yet, which is an answer.
    let grooming = asked[0].clone();
    f.manager
        .finish_run(
            &grooming,
            std::path::Path::new("/nonexistent/checkout"),
            grooming.created_at,
            baybo_project::RunOutcome {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
        )
        .await;
    tick(&f, &p.id).await;
    tick(&f, &p.id).await;
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "and a card nothing changed about is not raised again every pass"
    );
}

#[tokio::test]
async fn a_board_of_nothing_but_the_operators_backlog_is_left_alone() {
    let f = fixture().await;
    let (p, _dev) = driven_board(&f, 3).await;

    // The mirror of the test above with its agent-filed card taken away. That
    // card is what made Grooming fire there, and Grooming returning is what
    // kept the board from ever reaching the question below it.
    let theirs = park_card(&f, &p.id, "someday, maybe", IssueActor::User, None).await;

    tick(&f, &p.id).await;
    tick(&f, &p.id).await;
    assert!(
        f.dispatched.lock().is_empty(),
        "the operator's parked card is the whole board, and the board has \
         nothing to ask about it: {:?}",
        asks(&f)
    );
    assert_eq!(
        column_of(&f, &p.id, theirs.number).await,
        IssueStatus::Backlog,
        "and it is still where they left it"
    );
}

#[tokio::test]
async fn a_run_the_operator_stopped_is_not_countermanded_on_the_next_pass() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let card = queue_card(&f, &p.id, "the work", Some(dev), IssuePriority::None).await;

    tick(&f, &p.id).await;
    assert_eq!(asks(&f), vec![RunTrigger::Promoted], "it started");
    f.dispatched.lock().clear();

    f.manager
        .cancel_run(&p.id, card.number)
        .await
        .expect("the operator stops it");
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;
    tick(&f, &p.id).await;
    assert!(
        f.dispatched.lock().is_empty(),
        "a stop the operator set was countermanded on the next pass: {:?}",
        asks(&f)
    );
}

#[tokio::test]
async fn a_deferral_the_board_outlived_is_put_back_to_the_lead() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    // The shape this exists for. The board filed a card and parked it; the
    // lead looked and deferred it on the *other* card — a complete answer,
    // and one whose premise is nowhere on the card it was given about.
    let parked = park_card(
        &f,
        &p.id,
        "after the other one lands",
        IssueActor::Agent(dev.clone()),
        None,
    )
    .await;
    let working = queue_card(
        &f,
        &p.id,
        "the other one",
        Some(dev.clone()),
        IssuePriority::High,
    )
    .await;

    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f),
        vec![RunTrigger::Promoted, RunTrigger::Grooming],
        "the staffed card starts, and the parked one goes to the lead"
    );
    let (started, grooming) = {
        let dispatched = f.dispatched.lock();
        (dispatched[0].clone(), dispatched[1].clone())
    };
    settle_clean(&f, &grooming).await;

    // Nothing on the parked card changed, so nothing about it is asked
    // again — correctly, while the card it is waiting on is still running.
    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f).len(),
        2,
        "a deferral is an answer, and the board does not overrule it"
    );

    // The thing it was waiting for happens. It touches the parked card in
    // no way at all, which is the whole difficulty.
    f.manager
        .move_issue(
            &p.id,
            working.number,
            IssueActor::Agent(dev.clone()),
            IssueStatus::Done,
            &[working.number],
        )
        .await
        .expect("close it");
    settle_clean(&f, &started).await;

    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f),
        vec![
            RunTrigger::Promoted,
            RunTrigger::Grooming,
            RunTrigger::BoardIdle
        ],
        "work outlived the lead's last look and the board is empty behind \
         it — which is where the board used to stop for good"
    );
    let told = f.dispatched.lock()[2].clone();
    assert_eq!(
        told.number, parked.number,
        "a run is a row on a card, so the only live card is what it anchors to"
    );
    assert_eq!(
        told.agent_id, lead,
        "and the lead is who answers for a board"
    );

    // Told once. The lead answering is not the board working, so sitting
    // still is not a second question.
    settle_clean(&f, &told).await;
    tick(&f, &p.id).await;
    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f).len(),
        3,
        "and it is told once per drain, not per tick"
    );
}

/// A rule the board schedules by is news no card carries, and the board is
/// what gets told.
///
/// It used to re-open every **card's** question instead. That reads the
/// right way round — the answer was given under the old rules, so ask it
/// again — and it is wrong twice over. It asks the lead to decide one thing
/// about the whole board once per card, and, because the same stamp is what
/// the ask cap counted from, each save minted the quota to do it again: a
/// short burst of saves bought nine runs across five cards, all of which
/// changed nothing.
#[tokio::test]
async fn a_rule_the_board_changed_is_one_question_about_the_board() {
    let f = fixture().await;
    let (p, _dev) = driven_board(&f, 3).await;

    // A card in Review with nobody on it: the lead's question to arrange,
    // and the lead escalates it rather than answering — the board may not
    // land its own work, so there is nothing else it can say.
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Review,
                assignee: None,
                ..new_issue("approved, and going nowhere")
            },
        )
        .await
        .expect("card");
    tick(&f, &p.id).await;
    let review = f.dispatched.lock()[0].clone();
    assert_eq!(review.trigger, RunTrigger::Review);
    settle_clean(&f, &review).await;

    tick(&f, &p.id).await;
    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f),
        vec![RunTrigger::Review],
        "an unchanged card is not the same question twice"
    );

    // The operator turns on the one thing that answer was waiting for. It
    // touches no card, so nothing anywhere on the board records it.
    set_merge(&f, &p, true).await;
    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f),
        vec![RunTrigger::Review, RunTrigger::BoardIdle],
        "the premise of every standing answer moved, and the lead is handed \
         the board — not one card at a time"
    );

    // Told once. Answering is a look, so the rule the answer was given
    // under is a rule the lead has now read.
    let told = f.dispatched.lock()[1].clone();
    settle_clean(&f, &told).await;
    tick(&f, &p.id).await;
    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f).len(),
        2,
        "and saving the settings again buys nothing until something happens"
    );
    set_merge(&f, &p, false).await;
    set_merge(&f, &p, true).await;
    tick(&f, &p.id).await;
    assert_eq!(
        asks(&f).len(),
        3,
        "a rule that changes after that look is news again — once, not once \
         per save and not once per card"
    );
}

#[tokio::test]
async fn a_card_face_is_told_who_filed_it() {
    // The same fact the grooming question turns on, resolved once for
    // whoever draws a card: a face that answered it a second time could
    // mark a card the board will never ask about.
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;

    let theirs = park_card(&f, &p.id, "someday", IssueActor::User, None).await;
    let ours = park_card(&f, &p.id, "spun out", IssueActor::Agent(dev), None).await;

    let board = f.manager.board_cards(&p.id).await.expect("board");
    assert!(board.opened_by_agent(ours.number));
    assert!(
        !board.opened_by_agent(theirs.number),
        "the operator's own card is not the board's work breakdown"
    );
    assert!(
        !board.opened_by_agent(theirs.number + 999),
        "and a number this board does not have reads as nobody's"
    );
}

#[tokio::test]
async fn grooming_a_card_into_todo_is_what_starts_it() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let ours = park_card(
        &f,
        &p.id,
        "ready when somebody says so",
        IssueActor::Agent(dev.clone()),
        Some(dev.clone()),
    )
    .await;
    tick(&f, &p.id).await;
    let grooming = f.dispatched.lock()[0].clone();
    assert_eq!(grooming.trigger, RunTrigger::Grooming);

    // The lead answers by moving it up, from inside its own grooming run.
    f.manager
        .move_issue(
            &p.id,
            ours.number,
            IssueActor::Agent(grooming.agent_id.clone()),
            IssueStatus::Todo,
            &[ours.number],
        )
        .await
        .expect("groom");
    f.manager
        .finish_run(
            &grooming,
            std::path::Path::new("/nonexistent/checkout"),
            grooming.created_at,
            baybo_project::RunOutcome {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
        )
        .await;

    assert_eq!(tick(&f, &p.id).await, 1, "now the board has a card to pull");
    assert_eq!(
        column_of(&f, &p.id, ours.number).await,
        IssueStatus::InProgress
    );
    let runs = f.dispatched.lock().clone();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[1].trigger,
        RunTrigger::Promoted,
        "and it starts the way every other Todo card does"
    );
    assert_eq!(runs[1].agent_id, dev, "as the card's own assignee");
}

#[tokio::test]
async fn staffing_a_card_the_lead_was_asked_about_starts_it() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    queue_card(
        &f,
        &p.id,
        "somebody should do this",
        None,
        IssuePriority::None,
    )
    .await;
    tick(&f, &p.id).await;
    let triage = f.dispatched.lock()[0].clone();

    // The lead assigns it from inside its own triage run, so the card is
    // ready while its one run slot is still spoken for.
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::Agent(triage.agent_id.clone()),
            IssueUpdate {
                assignee: Some(Some(dev.clone())),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("assign");
    assert_eq!(tick(&f, &p.id).await, 0);
    assert_eq!(
        column_of(&f, &p.id, 1).await,
        IssueStatus::Todo,
        "promoting it now would move it into In Progress and start nothing"
    );

    f.manager
        .finish_run(
            &triage,
            std::path::Path::new("/nonexistent/checkout"),
            triage.created_at,
            baybo_project::RunOutcome {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
        )
        .await;
    assert_eq!(tick(&f, &p.id).await, 1);

    assert_eq!(
        column_of(&f, &p.id, 1).await,
        IssueStatus::InProgress,
        "the card the lead staffed starts as soon as its slot is free again"
    );
    let dispatched = f.dispatched.lock().clone();
    assert_eq!(dispatched.len(), 2);
    assert_eq!(dispatched[1].trigger, RunTrigger::Promoted);
    assert_eq!(dispatched[1].agent_id, dev);
}

/// Settle a run the way the waiter would, with nothing left behind.
async fn settle_clean(f: &Fixture, run: &baybo_store::project::IssueRunRow) {
    f.manager
        .finish_run(
            run,
            std::path::Path::new("/nonexistent/checkout"),
            run.created_at,
            baybo_project::RunOutcome {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
        )
        .await;
}

#[tokio::test]
async fn a_card_sitting_in_review_wakes_the_lead_once() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Review,
                assignee: Some(dev),
                ..new_issue("done, somebody look at it")
            },
        )
        .await
        .expect("card");
    tick(&f, &p.id).await;

    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "the lead is woken to arrange the review");
    assert_eq!(asked[0].trigger, RunTrigger::Review);
    assert_eq!(asked[0].agent_id, lead);

    // The lead reads it and leaves it alone, which is a legitimate answer.
    settle_clean(&f, &asked[0]).await;
    tick(&f, &p.id).await;
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "an unchanged review card is not the same question twice"
    );

    // The card changing under it is a new question.
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                description: Some("the acceptance notes moved".to_owned()),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("edit");
    tick(&f, &p.id).await;
    let dispatched = f.dispatched.lock().clone();
    assert_eq!(dispatched.len(), 2, "so the lead is asked again");
    assert_eq!(dispatched[1].trigger, RunTrigger::Review);
}

#[tokio::test]
async fn work_that_silently_stops_wakes_the_lead() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    queue_card(
        &f,
        &p.id,
        "wire the importer",
        Some(dev),
        IssuePriority::None,
    )
    .await;
    tick(&f, &p.id).await;
    let worked = f.dispatched.lock()[0].clone();
    assert_eq!(worked.trigger, RunTrigger::Promoted);

    // The run ends without the card moving anywhere: In Progress, no run,
    // nothing queued — the shape the hourly patrol used to catch.
    settle_clean(&f, &worked).await;
    tick(&f, &p.id).await;

    let dispatched = f.dispatched.lock().clone();
    assert_eq!(dispatched.len(), 2, "the lead is woken about the stall");
    assert_eq!(dispatched[1].trigger, RunTrigger::Stalled);
    assert_eq!(dispatched[1].agent_id, lead);

    settle_clean(&f, &dispatched[1]).await;
    tick(&f, &p.id).await;
    assert_eq!(
        f.dispatched.lock().len(),
        2,
        "and not woken again about a card it already looked at"
    );
}

#[tokio::test]
async fn a_blocked_cards_parked_run_waits_for_the_unblock() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("started and then paused")
            },
        )
        .await
        .expect("card");
    let started = f.dispatched.lock().clone();
    assert_eq!(started.len(), 1, "entering In Progress started it");

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(Some("waiting on the API".to_owned())),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("block");

    assert_eq!(
        f.manager.resume_unsettled_runs().await.expect("boot sweep"),
        0,
        "the boot sweep leaves a blocked card's queued run where it lies"
    );
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "nothing re-drove it while the block stood"
    );
    tick(&f, &p.id).await;
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "and a blocked card is not a stall the lead gets woken about"
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
            None,
        )
        .await
        .expect("unblock");
    let dispatched = f.dispatched.lock().clone();
    assert_eq!(dispatched.len(), 2, "the unblock hands the run back out");
    assert_eq!(
        dispatched[1].id, started[0].id,
        "the same parked row, not a new attempt"
    );
    assert_eq!(dispatched[1].agent_id, dev);
}

/// Held runs are work the board already owes, so they take the next slots —
/// and they have to be counted *as* slots, not released on top of them.
///
/// The bug this pins: a held run is not in `board_load.working`, so a pass
/// that counts slots before releasing sees an idle board. The first
/// promotion's `enqueue` then releases every hold on its way past the budget
/// gate, and the board runs the promotions *and* the holds at once.
///
/// The headroom is restored through the **store**, not `update_project`,
/// because that is the shape production actually has: `update_project`
/// releases what it un-parks itself, so the case where a tick is the first
/// thing to see the new headroom is the UTC day rolling over, which nothing
/// is notified about.
#[tokio::test]
async fn work_the_board_already_owes_takes_the_slots_a_rolled_over_budget_frees() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 2).await;
    set_budget(&f, &p, Some(baybo_model::MicroUsd::ZERO)).await;

    // Two cards dragged straight into In Progress: each records a run, and
    // each is parked because the board has nothing to spend.
    for number in [1, 2] {
        queue_card(
            &f,
            &p.id,
            &format!("owed {number}"),
            Some(dev.clone()),
            IssuePriority::None,
        )
        .await;
        let order: Vec<i64> = (1..=number).collect();
        f.manager
            .move_issue(
                &p.id,
                number,
                IssueActor::User,
                IssueStatus::InProgress,
                &order,
            )
            .await
            .expect("drag");
    }
    let statuses: Vec<RunStatus> = f
        .manager
        .active_runs(&p.id)
        .await
        .expect("runs")
        .iter()
        .map(|run| run.status)
        .collect();
    assert_eq!(
        statuses,
        vec![RunStatus::Held, RunStatus::Held],
        "both runs are owed and parked"
    );

    // A third card is staffed and waiting, and then the day rolls over.
    queue_card(&f, &p.id, "waiting", Some(dev), IssuePriority::None).await;
    f.store
        .update_project(
            &p.id,
            &ProjectUpdate {
                name: p.name.clone(),
                description: p.description.clone(),
                daily_budget: None,
                daily_budget_tokens: None,
                max_parallel_issue_runs: 2,
                agents_may_merge: false,
            },
        )
        .await
        .expect("headroom, without the manager releasing anything");

    tick(&f, &p.id).await;

    let live = f
        .manager
        .active_runs(&p.id)
        .await
        .expect("runs")
        .iter()
        .filter(|run| run.status != RunStatus::Held)
        .count();
    assert_eq!(
        live, 2,
        "the ceiling is 2 and the two owed runs fill it exactly"
    );
    assert_eq!(
        column_of(&f, &p.id, 3).await,
        IssueStatus::Todo,
        "the waiting card gets the next slot to come free, not one already promised"
    );
}

/// The property the whole design rests on: the driver is level-triggered, so
/// running it again is not a second helping. Nothing announces a card to it,
/// so nothing can announce one twice either — every pass just re-reads the
/// board and closes whatever gap it finds.
#[tokio::test]
async fn driving_a_board_again_does_not_start_the_same_card_twice() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 0).await;
    queue_card(
        &f,
        &p.id,
        "wire the importer",
        Some(dev),
        IssuePriority::None,
    )
    .await;
    assert_eq!(tick(&f, &p.id).await, 0, "no room yet");

    set_ceiling(&f, &p, 2).await;
    assert_eq!(tick(&f, &p.id).await, 1, "the ceiling made room");
    assert_eq!(column_of(&f, &p.id, 1).await, IssueStatus::InProgress);

    for _ in 0..3 {
        assert_eq!(
            tick(&f, &p.id).await,
            0,
            "the card is no longer in Todo, so later passes have nothing to do"
        );
    }
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        1,
        "one card, one run"
    );

    // The boot sweep runs alongside the driver and hands the same recorded
    // row back out; it must not become a second run either.
    f.dispatched.lock().clear();
    f.manager
        .resume_unsettled_runs()
        .await
        .expect("the boot sweep");
    assert!(
        f.dispatched
            .lock()
            .iter()
            .all(|run| run.number == 1 && run.trigger == RunTrigger::Promoted),
        "the boot sweep re-drives what was already recorded rather than recording more"
    );
    assert_eq!(f.manager.list_runs(&p.id, 1).await.expect("runs").len(), 1);
}

/// The sweep is the only thing that drives a board in production, so its
/// first tick has to be the boot pass — a board whose queue moved while the
/// process was down has nobody else left to notice.
#[tokio::test(start_paused = true)]
async fn the_sweep_drives_on_its_first_tick_and_stops_when_told_to() {
    let f = std::sync::Arc::new(fixture().await);
    let (p, dev) = driven_board(&f, 1).await;
    queue_card(
        &f,
        &p.id,
        "wire the importer",
        Some(dev),
        IssuePriority::None,
    )
    .await;

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let driver = {
        let f = std::sync::Arc::clone(&f);
        tokio::spawn(async move {
            f.manager
                .run_driver(async move {
                    let _ = stopped.await;
                })
                .await
        })
    };

    // Time is paused, so this costs no wall clock: it only hands the
    // runtime enough turns for the driver's first tick — which a
    // `tokio::time::interval` fires immediately, and which is what makes
    // the sweep its own boot pass — to finish its queries.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(
        column_of(&f, &p.id, 1).await,
        IssueStatus::InProgress,
        "the sweep started the card without anybody calling drive"
    );

    stop.send(()).expect("the driver is still listening");
    tokio::time::timeout(std::time::Duration::from_secs(1), driver)
        .await
        .expect("the driver stops on the shutdown signal rather than outliving it")
        .expect("no panic");
}

/// Store bytes and get the id that names them, the way an operator's upload
/// or an agent's `PutBlob` would.
async fn stored_blob(f: &Fixture, bytes: &[u8], mime: &str) -> String {
    f.blobs
        .put(bytes, mime, None)
        .await
        .expect("blob store accepts the bytes")
        .blob_id
}

#[tokio::test]
async fn a_card_carries_its_files_and_the_server_reads_their_type_off_the_store() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Files"))
        .await
        .expect("create");
    let blob = stored_blob(&f, b"\x89PNG\r\n\x1a\n", "image/png").await;

    let issue = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                attachments: vec![baybo_project::AttachmentRequest {
                    blob_id: blob.clone(),
                    filename: Some("mockup.png".to_owned()),
                }],
                ..new_issue("Redesign the header")
            },
        )
        .await
        .expect("create")
        .into_issue();

    assert_eq!(issue.attachments.len(), 1);
    let stored = &issue.attachments[0];
    assert_eq!(stored.blob_id, blob);
    assert_eq!(stored.filename.as_deref(), Some("mockup.png"));
    assert_eq!(
        stored.mime_type, "image/png",
        "the type comes off the store, not off anything a caller said"
    );
    assert_eq!(stored.size, 8, "and so does the size");

    // And it survives the round trip through sqlite's JSON column.
    let read = f
        .manager
        .get_issue(&p.id, issue.number)
        .await
        .expect("read back");
    assert_eq!(read.attachments, issue.attachments);
}

#[tokio::test]
async fn a_file_the_store_never_saw_is_refused_rather_than_recorded() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Files"))
        .await
        .expect("create");

    let refused = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                attachments: vec![baybo_project::AttachmentRequest {
                    // Well-formed and never stored: a card must not be opened
                    // pointing at bytes nobody has.
                    blob_id: format!("sha256:{}.{}", "a".repeat(64), "b".repeat(32)),
                    filename: Some("ghost.pdf".to_owned()),
                }],
                ..new_issue("Ghost")
            },
        )
        .await;

    assert!(
        matches!(
            refused,
            Err(baybo_project::ProjectError::Invalid { field, .. }) if field == "attachments"
        ),
        "expected a refusal naming the field, got {refused:?}"
    );
}

#[tokio::test]
async fn a_comment_may_be_nothing_but_a_file_but_never_nothing_at_all() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Files"))
        .await
        .expect("create");
    let issue = f
        .manager
        .create_issue(&p.id, IssueActor::User, new_issue("Look at this"))
        .await
        .expect("create")
        .into_issue();
    let blob = stored_blob(&f, b"screenshot", "image/png").await;

    let entry = f
        .manager
        .comment(
            &p.id,
            issue.number,
            IssueActor::User,
            "",
            &[baybo_project::AttachmentRequest {
                blob_id: blob.clone(),
                filename: Some("here.png".to_owned()),
            }],
        )
        .await
        .expect("\"here\" plus a picture is a real thing to say on a card");

    match entry.body {
        baybo_store::project::IssueEventBody::Comment { text, attachments } => {
            assert_eq!(text, "");
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].blob_id, blob);
            assert_eq!(attachments[0].mime_type, "image/png");
        }
        other => panic!("expected a comment, got {other:?}"),
    }

    let empty = f
        .manager
        .comment(&p.id, issue.number, IssueActor::User, "   ", &[])
        .await;
    assert!(
        matches!(
            empty,
            Err(baybo_project::ProjectError::Invalid { field, .. }) if field == "text"
        ),
        "no text and no files is still nothing to say, got {empty:?}"
    );
}

#[tokio::test]
async fn removing_a_cards_last_file_is_a_write_and_not_a_no_op() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Files"))
        .await
        .expect("create");
    let blob = stored_blob(&f, b"doc", "application/pdf").await;
    let issue = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                attachments: vec![baybo_project::AttachmentRequest {
                    blob_id: blob,
                    filename: Some("spec.pdf".to_owned()),
                }],
                ..new_issue("Spec")
            },
        )
        .await
        .expect("create")
        .into_issue();

    // An empty list is a *present* value meaning "no files" — a patch that
    // otherwise sets nothing, which the emptiness guard must not reject.
    let after = f
        .manager
        .update_issue(
            &p.id,
            issue.number,
            IssueActor::User,
            IssueUpdate::default(),
            Some(&[]),
        )
        .await
        .expect("clearing the list is a real edit");
    assert!(after.attachments.is_empty());
}

/// Build output under a checkout nobody is working in is regenerable, and
/// the sweep may take it — but only after every gate agrees.
#[tokio::test]
async fn an_idle_checkouts_build_output_is_reclaimed_and_a_busy_ones_is_not() {
    use baybo_project::BuildArtifacts;

    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Artifacts"))
        .await
        .expect("create");
    let dev = seed_agent(&f, &project.id, "dev-1", AgentFramework::Baybo).await;

    // Two cards, each with a real checkout carrying a `target/` big enough
    // to be worth reclaiming.
    let mut roots = Vec::new();
    for title in ["idle card", "busy card"] {
        let issue = f
            .manager
            .create_issue(
                &project.id,
                IssueActor::User,
                NewIssueRequest {
                    status: IssueStatus::Review,
                    assignee: Some(dev.clone()),
                    ..new_issue(title)
                },
            )
            .await
            .expect("create issue")
            .into_issue();
        let root = baybo_project::worktree::worktree_root(&f.paths, &project.id, issue.number);
        let branch = baybo_project::worktree::branch_name(issue.number, &issue.title);
        baybo_project::worktree::ensure(std::path::Path::new(&project.workdir), &root, &branch)
            .await
            .expect("cut a worktree the way a run would");
        let target = root.join("target");
        tokio::fs::create_dir_all(&target).await.expect("target");
        tokio::fs::write(target.join("blob.bin"), vec![0u8; 80 * 1024 * 1024])
            .await
            .expect("write build output");
        // The repository has to agree the directory is ignorable, which is
        // the gate that keeps a tracked `target/` alive.
        tokio::fs::write(root.join(".gitignore"), "/target\n")
            .await
            .expect("gitignore");
        roots.push((issue, root));
    }

    // The second card is mid-run: a queued row is one `git worktree add`
    // away from a build.
    let (busy_issue, busy_root) = &roots[1];
    f.manager
        .retry_run(&project.id, busy_issue.number)
        .await
        .expect("a run this card owes");

    let freed = f
        .manager
        .reclaim_idle_build_artifacts(std::time::Duration::from_secs(0))
        .await;

    let (_, idle_root) = &roots[0];
    assert!(
        !idle_root.join("target").exists(),
        "the idle checkout gives its build output back"
    );
    assert!(
        idle_root.join(".gitignore").exists(),
        "and nothing else in the tree is touched"
    );
    assert!(
        busy_root.join("target").exists(),
        "a card with a run owed keeps its cache — something is about to build in there"
    );
    assert_eq!(freed.dirs_removed, 1);
    assert!(freed.bytes_freed >= 80 * 1024 * 1024);

    // Nothing is idle *enough* under the real TTL.
    let again = f
        .manager
        .reclaim_idle_build_artifacts(std::time::Duration::from_secs(3 * 86_400))
        .await;
    assert_eq!(
        (again.dirs_removed, again.bytes_freed),
        (0, 0),
        "a checkout touched moments ago is not idle"
    );
}

#[tokio::test]
async fn a_resumed_run_says_it_was_interrupted_and_never_says_it_started_twice() {
    use baybo_store::project::IssueEventBody;

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
        .expect("issue");
    let run = f.manager.list_runs(&p.id, 1).await.expect("runs").remove(0);

    let session = SessionId::from("issue-1");
    assert!(
        f.manager.start_run(&run, &session).await.expect("claim"),
        "the first executor takes it"
    );

    assert_eq!(
        f.manager.resume_unsettled_runs().await.expect("boot sweep"),
        1,
        "the process went down under it, so it is handed back out"
    );
    let handed_back = f.dispatched.lock().last().cloned().expect("re-dispatched");
    assert_eq!(handed_back.resumes, 1);
    assert!(
        f.manager
            .start_run(&handed_back, &session)
            .await
            .expect("re-claim"),
        "and the next executor re-claims it"
    );

    let timeline = f.manager.timeline(&p.id, 1).await.expect("timeline");
    let started: Vec<&IssueEventRow> = timeline
        .iter()
        .filter(|e| matches!(e.body, IssueEventBody::RunStarted { .. }))
        .collect();
    assert_eq!(
        started.len(),
        1,
        "a re-claimed row is already on the card; saying `started` again is the duplicate this fixes"
    );
    let interrupted: Vec<&IssueEventRow> = timeline
        .iter()
        .filter(|e| matches!(e.body, IssueEventBody::RunInterrupted { .. }))
        .collect();
    assert_eq!(interrupted.len(), 1, "the interruption is what is new");
    match (&started[0].body, &interrupted[0].body) {
        (
            IssueEventBody::RunStarted {
                run_id: started_id, ..
            },
            IssueEventBody::RunInterrupted {
                run_id: interrupted_id,
                resumes,
                ..
            },
        ) => {
            assert_eq!(started_id, interrupted_id, "both address the same run");
            assert_eq!(*resumes, 1);
        }
        other => panic!("unexpected bodies: {other:?}"),
    }
    assert_eq!(interrupted[0].actor, IssueActor::System);
}

#[tokio::test]
async fn a_re_claimed_run_still_announces_that_it_is_running() {
    let f = fixture().await;
    let (tx, mut announced) = tokio::sync::mpsc::unbounded_channel();
    let manager = ProjectManager::new(
        Arc::clone(&f.store),
        Arc::clone(&f.agents),
        Arc::clone(&f.blobs),
        f.paths.clone(),
        Arc::new(RecordingEvents(tx)),
        baybo_project::no_dispatch(),
        baybo_project::no_stopper(),
    );
    let p = manager
        .create_project(new_project("watched"))
        .await
        .expect("project");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    manager
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

    let run = manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
    let session = SessionId::from("issue-1");
    manager
        .start_run(&run, &session)
        .await
        .expect("the first executor takes it");
    manager
        .resume_unsettled_runs()
        .await
        .expect("the process went down under it");
    let handed_back = manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
    assert_eq!(handed_back.resumes, 1, "this is the resumed row");

    while announced.try_recv().is_ok() {}
    manager
        .start_run(&handed_back, &session)
        .await
        .expect("the next executor re-claims it");

    let mut heard = Vec::new();
    while let Ok(one) = announced.try_recv() {
        heard.push(one);
    }
    assert!(
        heard.contains(&format!("run {} #1", p.id)),
        "the claim moved the row to running, and nothing else says so: {heard:?}"
    );

    let started = manager
        .timeline(&p.id, 1)
        .await
        .expect("timeline")
        .into_iter()
        .filter(|e| {
            matches!(
                e.body,
                baybo_store::project::IssueEventBody::RunStarted { .. }
            )
        })
        .count();
    assert_eq!(
        started, 1,
        "and the entry the card already carries is still not written twice"
    );
}

#[tokio::test]
async fn the_board_stops_resuming_a_run_it_has_already_resumed_twice() {
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
                ..new_issue("wedged")
            },
        )
        .await
        .expect("issue");
    let run = f.manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
    let session = SessionId::from("issue-1");
    f.manager.start_run(&run, &session).await.expect("claim");
    f.dispatched.lock().clear();

    let mut re_driven = 0;
    for _ in 0..3 {
        re_driven += f.manager.resume_unsettled_runs().await.expect("boot sweep");
        let live = f.manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
        if live.status == RunStatus::Queued {
            f.manager
                .start_run(&live, &session)
                .await
                .expect("re-claim");
        }
    }

    assert_eq!(
        re_driven, 2,
        "two resurrections, and then the board stops paying for a third"
    );
    let settled = f.manager.list_runs(&p.id, 1).await.expect("runs").remove(0);
    assert_eq!(settled.status, RunStatus::Failed);
    assert!(
        settled
            .error
            .as_deref()
            .expect("the card is told why")
            .contains("stopped resuming it"),
        "the give-up says the board gave up, not that the run failed on its own: {:?}",
        settled.error
    );
    assert!(
        f.dispatched.lock().len() <= 2,
        "and nothing went out on the pass that gave up"
    );
}

#[tokio::test]
async fn an_agent_blocking_a_card_wakes_the_lead_and_marks_it_unread() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    let card = f
        .manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Review,
                assignee: Some(dev.clone()),
                ..new_issue("the goal contradicts the Go spec")
            },
        )
        .await
        .expect("card")
        .into_issue();
    f.dispatched.lock().clear();

    f.manager
        .update_issue(
            &p.id,
            card.number,
            IssueActor::Agent(dev),
            IssueUpdate {
                blocked_reason: Some(Some(
                    "the card asks for behaviour the language spec forbids — which wins?"
                        .to_owned(),
                )),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("the agent refuses to code and asks");

    tick(&f, &p.id).await;

    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "somebody has to answer a blocked card");
    assert_eq!(asked[0].trigger, RunTrigger::Blocked);
    assert_eq!(asked[0].agent_id, lead, "and that somebody is the lead");

    let cards = f.manager.board_cards(&p.id).await.expect("board");
    assert!(
        cards.signals(&card.id).unread >= 1,
        "an agent's block is news to the operator, exactly as an agent's move to Review is"
    );
}

#[tokio::test]
async fn a_blocked_card_is_asked_about_before_a_card_merely_waiting_for_review() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;

    for (number, title) in [(1, "waiting on a review"), (2, "stopped by a question")] {
        f.manager
            .create_issue(
                &p.id,
                IssueActor::User,
                NewIssueRequest {
                    status: IssueStatus::Review,
                    assignee: Some(dev.clone()),
                    ..new_issue(title)
                },
            )
            .await
            .expect("card");
        assert_eq!(
            number,
            f.manager.list_issues(&p.id).await.expect("issues").len() as i64
        );
    }
    f.manager
        .update_issue(
            &p.id,
            2,
            IssueActor::Agent(dev),
            IssueUpdate {
                blocked_reason: Some(Some(
                    "which of the two contradictory goals wins?".to_owned(),
                )),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("block");
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;
    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "one question per pass");
    assert_eq!(asked[0].number, 2);
    assert_eq!(
        asked[0].trigger,
        RunTrigger::Blocked,
        "review work is revisited by other machinery; a blocked card is not"
    );
}

async fn block_as_agent(f: &Fixture, project: &ProjectId, number: i64, by: &AgentProfileId) {
    f.manager
        .update_issue(
            project,
            number,
            IssueActor::Agent(by.clone()),
            IssueUpdate {
                blocked_reason: Some(Some(
                    "the card asks for behaviour the Go spec forbids — which wins?".to_owned(),
                )),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("block");
}

async fn unblock(f: &Fixture, project: &ProjectId, number: i64) {
    f.manager
        .update_issue(
            project,
            number,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(None),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("unblock");
}

#[tokio::test]
async fn a_block_that_landed_on_a_running_card_still_reaches_the_lead() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("blocked while it was running")
            },
        )
        .await
        .expect("card");
    let working = f.dispatched.lock().first().cloned().expect("started");
    f.manager
        .start_run(&working, &SessionId::from("issue-1"))
        .await
        .expect("the executor takes it");

    block_as_agent(&f, &p.id, 1, &dev).await;
    assert_eq!(
        f.manager.resume_unsettled_runs().await.expect("boot sweep"),
        0,
        "the sweep requeues the row and then leaves it: the card is blocked"
    );
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;

    let asked = f.dispatched.lock().clone();
    assert_eq!(
        asked.len(),
        1,
        "a blocked card is the one card nothing else on the board comes back to"
    );
    assert_eq!(asked[0].trigger, RunTrigger::Blocked);
    assert_eq!(asked[0].agent_id, lead, "and the lead is who answers it");

    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    let parked = runs
        .iter()
        .find(|run| run.id == working.id)
        .expect("the interrupted row is still in the log");
    assert_eq!(
        parked.status,
        RunStatus::Cancelled,
        "a card holds one unsettled run, so the row the block parked stood down for the question"
    );
    assert!(
        parked
            .error
            .as_deref()
            .is_some_and(|why| why.contains("stood down")),
        "and the card says why rather than showing a bare cancel: {:?}",
        parked.error
    );
    assert_eq!(
        column_of(&f, &p.id, 1).await,
        IssueStatus::InProgress,
        "standing the run down is not the board giving the card up"
    );

    tick(&f, &p.id).await;
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "and the question is asked once, not on every tick"
    );
}

#[tokio::test]
async fn a_lead_that_cannot_run_costs_the_card_nothing() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("blocked while it was running")
            },
        )
        .await
        .expect("card");
    let working = f.dispatched.lock().first().cloned().expect("started");
    f.manager
        .start_run(&working, &SessionId::from("issue-1"))
        .await
        .expect("the executor takes it");
    block_as_agent(&f, &p.id, 1, &dev).await;
    assert_eq!(
        f.manager.resume_unsettled_runs().await.expect("boot sweep"),
        0,
        "the sweep requeues the row and leaves it parked on the block"
    );

    f.agents
        .update(
            &lead,
            &baybo_store::AgentProfileUpdate {
                description: String::new(),
                framework: AgentFramework::Codex,
            },
        )
        .await
        .expect("the operator moves the lead onto codex");
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;

    assert!(
        f.dispatched.lock().is_empty(),
        "a lead that cannot host a session answers no question"
    );
    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    assert_eq!(runs.len(), 1, "and nothing was recorded in its place");
    assert_eq!(runs[0].id, working.id);
    assert_eq!(
        runs[0].status,
        RunStatus::Queued,
        "so the card keeps the run the block parked: the stand-down is the last \
         irreversible step, never the first"
    );
    assert!(runs[0].settled_at.is_none());
}

#[tokio::test]
async fn a_budget_held_row_stands_down_for_the_block_question_too() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 2).await;
    let lead = lead_of(&f, &p.id).await;
    set_budget(&f, &p, Some(baybo_model::MicroUsd::ZERO)).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("owed, and then blocked")
            },
        )
        .await
        .expect("card");
    let held = f
        .manager
        .active_runs(&p.id)
        .await
        .expect("runs")
        .into_iter()
        .next()
        .expect("the board recorded the work it owes");
    assert_eq!(
        held.status,
        RunStatus::Held,
        "the board has nothing to spend"
    );
    block_as_agent(&f, &p.id, 1, &dev).await;

    f.store
        .update_project(
            &p.id,
            &ProjectUpdate {
                name: p.name.clone(),
                description: p.description.clone(),
                daily_budget: None,
                daily_budget_tokens: None,
                max_parallel_issue_runs: p.max_parallel_issue_runs,
                agents_may_merge: false,
            },
        )
        .await
        .expect("headroom");
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;

    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    let stood_down = runs
        .iter()
        .find(|run| run.id == held.id)
        .expect("the held row is still in the log");
    assert_eq!(
        stood_down.status,
        RunStatus::Cancelled,
        "the hold was the card's one unsettled row, and the question needs that slot"
    );
    assert!(
        stood_down
            .error
            .as_deref()
            .is_some_and(|why| why.contains("stood down")),
        "and the card says why: {:?}",
        stood_down.error
    );
    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "so the lead is asked about the block");
    assert_eq!(
        (asked[0].trigger, &asked[0].agent_id),
        (RunTrigger::Blocked, &lead)
    );
}

#[tokio::test]
async fn the_answer_left_while_a_card_was_blocked_reaches_its_assignee() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Adjudicated"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("which version of the endpoint?")
            },
        )
        .await
        .expect("card");
    let working = f.dispatched.lock().first().cloned().expect("started");
    let briefed_at = Utc::now();
    f.manager
        .start_run(&working, &SessionId::from("issue-1"))
        .await
        .expect("the executor takes it");
    block_as_agent(&f, &p.id, 1, &dev).await;

    assert_eq!(
        f.manager
            .comment_delivery(&p.id, 1)
            .await
            .expect("delivery"),
        baybo_project::CommentDelivery::ParkedByABlock,
    );
    f.manager
        .comment(&p.id, 1, IssueActor::User, "use v2 of the endpoint", &[])
        .await
        .expect("the operator answers the question");
    finish(&f, &working, briefed_at, done()).await;
    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "nothing is woken while the block stands, not even the follow-up the run owes"
    );

    unblock(&f, &p.id, 1).await;

    let dispatched = f.dispatched.lock().clone();
    assert_eq!(
        dispatched.len(),
        2,
        "so the unblock is the door that delivers what arrived while the card was parked"
    );
    assert_eq!(dispatched[1].trigger, RunTrigger::Comment);
    assert_eq!(
        dispatched[1].agent_id, dev,
        "to the assignee, who is who was waiting for it"
    );

    unblock(&f, &p.id, 1).await;
    assert_eq!(
        f.dispatched.lock().len(),
        2,
        "and an unblock with nothing said under it starts nothing"
    );
}

#[tokio::test]
async fn a_lead_lifting_a_block_mid_run_delivers_what_was_said_while_the_card_was_parked() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("which version of the endpoint?")
            },
        )
        .await
        .expect("card");
    let working = f.dispatched.lock().first().cloned().expect("started");
    let briefed_at = Utc::now();
    f.manager
        .start_run(&working, &SessionId::from("issue-1"))
        .await
        .expect("the executor takes it");
    block_as_agent(&f, &p.id, 1, &dev).await;
    f.manager
        .comment(&p.id, 1, IssueActor::User, "use v2 of the endpoint", &[])
        .await
        .expect("the operator answers the question");
    finish(&f, &working, briefed_at, done()).await;

    tick(&f, &p.id).await;
    let ask = f
        .dispatched
        .lock()
        .get(1)
        .cloned()
        .expect("the lead is woken");
    assert_eq!(ask.trigger, RunTrigger::Blocked);
    let lead_briefed_at = Utc::now();
    f.manager
        .start_run(&ask, &SessionId::from("lead-1"))
        .await
        .expect("the lead takes its question");
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::Agent(lead.clone()),
            IssueUpdate {
                blocked_reason: Some(None),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("the lead lifts the block from inside its own run");
    assert_eq!(
        f.dispatched.lock().len(),
        2,
        "nothing may be enqueued behind a row that is still running"
    );

    finish(&f, &ask, lead_briefed_at, done()).await;

    let dispatched = f.dispatched.lock().clone();
    assert_eq!(
        dispatched.len(),
        3,
        "so the settle of the run that lifted the block is what hands the answer over"
    );
    assert_eq!(dispatched[2].trigger, RunTrigger::Comment);
    assert_eq!(
        dispatched[2].agent_id, dev,
        "to the assignee, who is who was waiting for it"
    );

    tick(&f, &p.id).await;
    assert_eq!(
        f.dispatched.lock().len(),
        3,
        "and once: the card is being worked again, not stalled"
    );
}

#[tokio::test]
async fn one_write_that_unblocks_and_starts_work_dispatches_one_run() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Handed over"))
        .await
        .expect("p");
    let first = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    let second = seed_agent(&f, &p.id, "dev-2", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(first.clone()),
                ..new_issue("blocked, then handed over")
            },
        )
        .await
        .expect("card");
    let started = f.dispatched.lock().first().cloned().expect("started");
    f.store_settle(&started.id, RunStatus::Done).await;
    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                blocked_reason: Some(Some("waiting on the operator".to_owned())),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("block");
    f.dispatched.lock().clear();

    f.manager
        .update_issue(
            &p.id,
            1,
            IssueActor::User,
            IssueUpdate {
                assignee: Some(Some(second.clone())),
                blocked_reason: Some(None),
                ..IssueUpdate::default()
            },
            None,
        )
        .await
        .expect("one write hands the card over and lifts the block");

    let dispatched = f.dispatched.lock().clone();
    assert_eq!(
        dispatched.len(),
        1,
        "a row this write itself recorded is not a row the block parked: {:?}",
        dispatched.iter().map(|run| &run.id).collect::<Vec<_>>()
    );
    assert_eq!(dispatched[0].agent_id, second);
    assert_eq!(dispatched[0].trigger, RunTrigger::Assigned);
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        2,
        "and the ledger holds one row per run, as it always did"
    );
}

#[tokio::test]
async fn a_retry_does_not_start_a_run_a_block_has_stopped() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Paused"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::InProgress,
                assignee: Some(dev.clone()),
                ..new_issue("stopped with a question on it")
            },
        )
        .await
        .expect("card");
    let started = f.dispatched.lock().first().cloned().expect("started");
    f.store_settle(&started.id, RunStatus::Done).await;
    block_as_agent(&f, &p.id, 1, &dev).await;
    f.dispatched.lock().clear();

    match f.manager.retry_run(&p.id, 1).await {
        Err(ProjectError::Invalid { field, reason }) => {
            assert_eq!(field, "issue");
            assert_eq!(
                reason,
                "this issue is blocked — lift the block before running it again"
            );
        }
        other => panic!("a blocked card should have refused the retry: {other:?}"),
    }
    assert!(
        f.dispatched.lock().is_empty(),
        "and nothing went out on a card somebody stopped"
    );
    assert_eq!(
        f.manager.list_runs(&p.id, 1).await.expect("runs").len(),
        1,
        "nor was a row recorded to sit there parked"
    );

    unblock(&f, &p.id, 1).await;
    let again = f.manager.retry_run(&p.id, 1).await.expect("retry");
    assert_eq!(
        again.trigger,
        RunTrigger::Retry,
        "the one write that lifts the block is what makes the card runnable again"
    );
    assert_eq!(f.dispatched.lock().len(), 1);
}

#[tokio::test]
async fn a_mention_puts_nobody_on_a_card_a_block_has_stopped() {
    let f = fixture().await;
    let p = f
        .manager
        .create_project(new_project("Paused"))
        .await
        .expect("p");
    let dev = seed_agent(&f, &p.id, "dev-1", AgentFramework::Baybo).await;
    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Todo,
                ..new_issue("nobody on it yet")
            },
        )
        .await
        .expect("card");
    block_as_agent(&f, &p.id, 1, &dev).await;

    f.manager
        .comment(&p.id, 1, IssueActor::User, "@dev-1 can you take this?", &[])
        .await
        .expect("the mention is recorded like any other comment");

    assert!(
        f.manager
            .get_issue(&p.id, 1)
            .await
            .expect("card")
            .assignee
            .is_none(),
        "a passing mention does not staff a card somebody paused"
    );
    assert!(f.dispatched.lock().is_empty(), "and starts nothing");

    unblock(&f, &p.id, 1).await;
    f.manager
        .comment(&p.id, 1, IssueActor::User, "@dev-1 can you take this?", &[])
        .await
        .expect("comment");
    assert_eq!(
        f.manager
            .get_issue(&p.id, 1)
            .await
            .expect("card")
            .assignee
            .as_ref(),
        Some(&dev),
        "and the same words land the moment the block is gone"
    );
    assert_eq!(f.dispatched.lock().len(), 1, "waking whoever they named");
}

#[tokio::test]
async fn the_leads_answer_on_a_card_it_left_blocked_starts_no_work() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Review,
                assignee: Some(dev.clone()),
                ..new_issue("the goal contradicts the Go spec")
            },
        )
        .await
        .expect("card");
    block_as_agent(&f, &p.id, 1, &dev).await;
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;
    let ask = f
        .dispatched
        .lock()
        .first()
        .cloned()
        .expect("the lead is woken");
    assert_eq!(ask.trigger, RunTrigger::Blocked);
    let briefed_at = Utc::now();
    f.manager
        .start_run(&ask, &SessionId::from("lead-1"))
        .await
        .expect("the lead takes its question");

    assert_eq!(
        f.manager
            .comment_delivery(&p.id, 1)
            .await
            .expect("delivery"),
        baybo_project::CommentDelivery::ParkedByABlock,
        "and the composer says so before anybody writes a word"
    );
    f.manager
        .comment(
            &p.id,
            1,
            IssueActor::Agent(lead.clone()),
            "the spec wins — say so on the card and I will decide the rest",
            &[],
        )
        .await
        .expect("the lead hands it back, and leaves the block standing");
    finish(&f, &ask, briefed_at, done()).await;

    assert_eq!(
        f.dispatched.lock().len(),
        1,
        "nothing but the question itself: a card the block stopped takes no work run"
    );
    let runs = f.manager.list_runs(&p.id, 1).await.expect("runs");
    assert!(
        runs.iter().all(|run| run.agent_id == lead),
        "and the assignee was never put back on it: {runs:#?}"
    );
}

#[tokio::test]
async fn a_finished_card_that_still_carries_a_block_does_not_swallow_the_other_questions() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let lead = lead_of(&f, &p.id).await;

    for (number, title) in [(1, "blocked and then finished"), (2, "waiting on a review")] {
        f.manager
            .create_issue(
                &p.id,
                IssueActor::User,
                NewIssueRequest {
                    status: IssueStatus::Review,
                    assignee: Some(dev.clone()),
                    ..new_issue(title)
                },
            )
            .await
            .expect("card");
        assert_eq!(
            number,
            f.manager.list_issues(&p.id).await.expect("issues").len() as i64
        );
    }
    block_as_agent(&f, &p.id, 1, &dev).await;
    f.manager
        .move_issue(&p.id, 1, IssueActor::User, IssueStatus::Done, &[1])
        .await
        .expect("the operator finishes the card without lifting the block");
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;

    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "the pass still asks its one question");
    assert_eq!(
        (asked[0].number, asked[0].trigger),
        (2, RunTrigger::Review),
        "a card no run can start on is not a question the lead can answer, \
         and it does not get to stop the ones behind it"
    );
    assert_eq!(asked[0].agent_id, lead);
}

async fn leadless_board(f: &Fixture, name: &str) -> (ProjectRow, AgentProfileId) {
    let now = Utc::now();
    let row = ProjectRow {
        id: ProjectId::generate(),
        name: name.to_owned(),
        description: String::new(),
        workdir: f.paths.work_dir().join(name).display().to_string(),
        daily_budget: None,
        daily_budget_tokens: None,
        max_parallel_issue_runs: 3,
        rules_changed_at: now,
        archived_at: None,
        created_at: now,
        updated_at: now,
        agents_may_merge: false,
    };
    f.store.create_project(&row).await.expect("legacy board");
    let dev = seed_agent(f, &row.id, "leader", AgentFramework::Baybo).await;
    (row, dev)
}

#[tokio::test]
async fn a_board_with_no_lead_is_given_one_before_it_is_asked_anything() {
    let f = fixture().await;
    let (p, dev) = leadless_board(&f, "predates-the-seed").await;
    assert!(
        f.manager
            .team(&p.id)
            .await
            .expect("team")
            .iter()
            .all(|row| row
                .team
                .as_ref()
                .is_none_or(|t| t.handle.as_str() != baybo_project::LEAD_HANDLE)),
        "the control: this board has no coordinator at all"
    );

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Review,
                assignee: Some(dev),
                ..new_issue("waiting on a review nobody can arrange")
            },
        )
        .await
        .expect("card");
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;

    let seeded = lead_of(&f, &p.id).await;
    let asked = f.dispatched.lock().clone();
    assert_eq!(asked.len(), 1, "and the question it could not ask went out");
    assert_eq!(asked[0].trigger, RunTrigger::Review);
    assert_eq!(asked[0].agent_id, seeded);
}

#[tokio::test]
async fn a_board_whose_lead_handle_is_reserved_says_so_once_instead_of_looping() {
    let f = fixture().await;
    let (p, dev) = driven_board(&f, 3).await;
    let original = lead_of(&f, &p.id).await;
    f.agents
        .remove_from_team(&original)
        .await
        .expect("tombstone it");

    f.manager
        .create_issue(
            &p.id,
            IssueActor::User,
            NewIssueRequest {
                status: IssueStatus::Review,
                assignee: Some(dev),
                ..new_issue("waiting on a review nobody can arrange")
            },
        )
        .await
        .expect("card");
    f.dispatched.lock().clear();

    tick(&f, &p.id).await;
    tick(&f, &p.id).await;

    assert!(
        f.dispatched.lock().is_empty(),
        "there is nobody to ask, and the board says so rather than inventing one"
    );
    assert!(
        f.manager
            .team(&p.id)
            .await
            .expect("team")
            .iter()
            .all(|row| row
                .team
                .as_ref()
                .is_none_or(|t| t.handle.as_str() != baybo_project::LEAD_HANDLE)),
        "and no second lead was conjured"
    );
}

#[tokio::test]
async fn a_card_filed_from_another_marks_the_origin_without_becoming_its_step() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Spun out"))
        .await
        .expect("create");
    let origin = f
        .manager
        .create_issue(&project.id, IssueActor::User, new_issue("HIR lowering"))
        .await
        .expect("create origin")
        .into_issue();

    let filed = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            NewIssueRequest {
                filed_from: Some(origin.id.clone()),
                ..new_issue("a parse bug the review turned up")
            },
        )
        .await
        .expect("create filed")
        .into_issue();

    assert_eq!(
        filed.filed_from,
        Some(origin.id.clone()),
        "the card carries where it came from"
    );
    assert_eq!(
        f.manager
            .timeline(&project.id, origin.number)
            .await
            .expect("origin timeline")
            .iter()
            .map(|e| e.body.kind())
            .collect::<Vec<_>>(),
        vec!["opened", "filed"],
        "and the origin stops going silent when its work spins work out"
    );
    assert!(
        matches!(
            &f.manager
                .timeline(&project.id, origin.number)
                .await
                .expect("origin timeline")[1]
                .body,
            baybo_store::project::IssueEventBody::Filed { number } if *number == filed.number
        ),
        "the entry names the card that was filed"
    );
    assert_eq!(
        f.manager
            .timeline(&project.id, filed.number)
            .await
            .expect("filed timeline")
            .iter()
            .map(|e| e.body.kind())
            .collect::<Vec<_>>(),
        vec!["opened"],
        "the reverse direction is the only one recorded — the card itself already says it"
    );

    assert!(
        filed.parent_issue_id.is_none(),
        "provenance is not hierarchy"
    );
    assert!(
        f.manager
            .children(&project.id, origin.number)
            .await
            .expect("children")
            .is_empty(),
        "and the origin gained no step, so nothing of its own now waits on this card"
    );
}

#[tokio::test]
async fn an_origin_on_another_board_is_refused() {
    let f = fixture().await;
    let mine = f
        .manager
        .create_project(new_project("Mine"))
        .await
        .expect("create mine");
    let theirs = f
        .manager
        .create_project(new_project("Theirs"))
        .await
        .expect("create theirs");
    let stranger = f
        .manager
        .create_issue(&theirs.id, IssueActor::User, new_issue("not your card"))
        .await
        .expect("create stranger")
        .into_issue();

    let err = f
        .manager
        .create_issue(
            &mine.id,
            IssueActor::User,
            NewIssueRequest {
                filed_from: Some(stranger.id.clone()),
                ..new_issue("reaching across")
            },
        )
        .await
        .expect_err("one board's card cannot be another board's origin");
    assert!(matches!(err, ProjectError::Invalid { .. }), "{err:?}");
    assert_eq!(
        f.manager
            .timeline(&theirs.id, stranger.number)
            .await
            .expect("stranger timeline")
            .len(),
        1,
        "and the refusal wrote nothing on the card it named"
    );
}

#[tokio::test]
async fn a_repeat_of_a_scheduled_check_files_nothing_on_its_origin() {
    let f = fixture().await;
    let project = f
        .manager
        .create_project(new_project("Daily"))
        .await
        .expect("create");
    let origin = f
        .manager
        .create_issue(
            &project.id,
            IssueActor::User,
            new_issue("the standing check"),
        )
        .await
        .expect("create origin")
        .into_issue();

    for _ in 0..2 {
        f.manager
            .create_issue(
                &project.id,
                IssueActor::User,
                NewIssueRequest {
                    source_key: Some("cron:nightly".to_owned()),
                    filed_from: Some(origin.id.clone()),
                    ..new_issue("the recurring finding")
                },
            )
            .await
            .expect("open or find the standing card");
    }

    assert_eq!(
        f.manager
            .timeline(&project.id, origin.number)
            .await
            .expect("origin timeline")
            .iter()
            .filter(|e| e.body.kind() == "filed")
            .count(),
        1,
        "a dedupe hit opened no card, so it filed none — 365 notes on one card is the bug this \
         guards"
    );
}
