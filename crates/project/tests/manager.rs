//! Domain-level tests for [`ProjectManager`]: what a write has to satisfy
//! before it lands, and the workdir guard that keeps a project's checkout
//! out of baybo's own workspace.

use std::sync::Arc;

use baybo_project::{NewIssueRequest, NewProject, ProjectError, ProjectManager};
use baybo_store::project::{IssuePriority, IssueStatus, IssueUpdate, ProjectUpdate};
use baybo_workspace::WorkspacePaths;

struct Fixture {
    manager: ProjectManager,
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
    Fixture {
        manager: ProjectManager::new(Arc::clone(&store.project), paths.clone()),
        paths,
        _workspace: workspace,
    }
}

fn new_project(name: &str) -> NewProject {
    NewProject {
        name: name.to_owned(),
        description: String::new(),
        workdir: None,
    }
}

fn new_issue(title: &str) -> NewIssueRequest {
    NewIssueRequest {
        title: title.to_owned(),
        description: String::new(),
        status: IssueStatus::Backlog,
        priority: IssuePriority::None,
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
        .create_issue(&project.id, new_issue("before"))
        .await
        .expect("create issue");
    f.manager
        .set_project_archived(&project.id, true)
        .await
        .expect("archive");

    let refused = f
        .manager
        .create_issue(&project.id, new_issue("after"))
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
        .create_issue(&project.id, new_issue("after restore"))
        .await
        .expect("writable again");
}

#[tokio::test]
async fn issues_answer_only_within_their_own_project() {
    let f = fixture().await;
    let a = f.manager.create_project(new_project("a")).await.expect("a");
    let b = f.manager.create_project(new_project("b")).await.expect("b");
    f.manager
        .create_issue(&a.id, new_issue("a's first"))
        .await
        .expect("issue");

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
        .create_issue(&p.id, new_issue("something"))
        .await
        .expect("issue");

    let refused = f
        .manager
        .update_issue(&p.id, 1, IssueUpdate::default())
        .await
        .expect_err("an empty patch is a caller mistake, not a no-op write");
    assert!(matches!(refused, ProjectError::Invalid { .. }));

    // A whitespace-only block reason reads as an unblock, not a blank block.
    let issue = f
        .manager
        .update_issue(
            &p.id,
            1,
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
            .create_issue(&p.id, new_issue(title))
            .await
            .expect("issue");
    }

    // `ordered_numbers` is the destination column's new contents, so a list
    // that omits the moved card would leave it unplaced.
    let refused = f
        .manager
        .move_issue(&p.id, 1, IssueStatus::Todo, &[2])
        .await
        .expect_err("the moved issue must appear in its destination");
    assert!(matches!(refused, ProjectError::Invalid { .. }));

    let moved = f
        .manager
        .move_issue(&p.id, 1, IssueStatus::Todo, &[1])
        .await
        .expect("move");
    assert_eq!(moved.status, IssueStatus::Todo);
    assert_eq!(moved.position, 0);
}
