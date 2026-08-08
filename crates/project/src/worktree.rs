//! The checkout an issue's run works in.

use std::path::{Path, PathBuf};

use baybo_model::ProjectId;
use baybo_workspace::WorkspacePaths;

use crate::error::{ProjectError, Result};

/// Where a run executes, and what it must be able to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The worktree — the run's working directory, and where its edits
    /// land.
    pub root: PathBuf,
    /// The project's repository. Writable because that is where a commit
    /// from the worktree actually goes; see the module docs.
    pub repo: PathBuf,
}

/// The directory an issue's worktree lives in.
pub fn worktree_root(paths: &WorkspacePaths, project: &ProjectId, number: i64) -> PathBuf {
    paths
        .work_dir()
        .join(WORKTREES_DIR)
        .join(project.as_str())
        .join(number.to_string())
}

const WORKTREES_DIR: &str = ".worktrees";

/// `issue/<number>-<slug>`, the branch a run's commits land on.
pub fn branch_name(number: i64, title: &str) -> String {
    let mut slug = String::with_capacity(title.len().min(MAX_SLUG_CHARS));
    for ch in title.chars() {
        if slug.len() >= MAX_SLUG_CHARS {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    match slug.trim_matches('-') {
        "" => format!("issue/{number}"),
        slug => format!("issue/{number}-{slug}"),
    }
}

const MAX_SLUG_CHARS: usize = 40;

/// Give sandboxed git commands somebody to be.
pub async fn ensure_commit_identity(work_dir: &Path) -> Result<()> {
    const IDENTITY: &str = r#"# Written by baybo so agent runs can commit. Yours to edit.
[user]
	name = baybo
	email = baybo@localhost
"#;
    let file = work_dir.join(".gitconfig");
    if file.exists() {
        return Ok(());
    }
    tokio::fs::create_dir_all(work_dir).await.map_err(|e| {
        ProjectError::Workdir(anyhow::anyhow!("create {}: {e}", work_dir.display()))
    })?;
    tokio::fs::write(&file, IDENTITY)
        .await
        .map_err(|e| ProjectError::Workdir(anyhow::anyhow!("write {}: {e}", file.display())))?;
    Ok(())
}

/// Open the issue's checkout, creating the worktree on first use.
pub async fn ensure(repo: &Path, root: &Path, branch: &str) -> Result<Checkout> {
    if !repo.join(".git").exists() {
        return Err(ProjectError::Workdir(anyhow::anyhow!(
            "{} is not a git repository; the project's workdir must be one",
            repo.display()
        )));
    }
    let checkout = Checkout {
        root: root.to_path_buf(),
        repo: repo.to_path_buf(),
    };
    if root.join(".git").exists() {
        return Ok(checkout);
    }
    if let Some(parent) = root.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            ProjectError::Workdir(anyhow::anyhow!("create {}: {e}", parent.display()))
        })?;
    }

    let existing = branch_exists(repo, branch).await?;
    let root_str = root.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["worktree", "add", "--quiet"];
    if !existing {
        args.extend(["-b", branch]);
    }
    args.push(&root_str);
    if existing {
        args.push(branch);
    }
    if git(repo, &args).await.is_err() {
        git(repo, &["worktree", "prune"]).await?;
        git(repo, &args).await?;
    }
    Ok(checkout)
}

/// The branch an existing worktree is actually on.
pub async fn branch_of(root: &Path) -> Option<String> {
    if !root.join(".git").exists() {
        return None;
    }
    let out = run(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// What happened when an issue's worktree was reclaimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reclaimed {
    /// The checkout is gone. `branch_deleted` is true when the branch held
    /// nothing the repository did not already have and git agreed to drop
    /// it. Anything either side is unsure about is left behind.
    Removed { branch_deleted: bool },
    /// Left alone, with the reason to put on the timeline. Uncommitted work
    /// is the common one: deleting it would destroy the only copy.
    Kept { reason: String },
    /// There was nothing there — already reclaimed, or never created.
    Absent,
}

/// Give an issue's worktree back when the issue is finished.
pub async fn reclaim(repo: &Path, root: &Path, branch: &str) -> Result<Reclaimed> {
    if !root.exists() {
        // A stale admin record can outlive the directory; tidying it here
        // keeps `git worktree list` honest and costs nothing.
        let _ = git(repo, &["worktree", "prune"]).await;
        return Ok(Reclaimed::Absent);
    }
    let root_str = root.to_string_lossy().into_owned();
    let out = run(repo, &["worktree", "remove", &root_str]).await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        // git exits 255 *after* deleting the checkout when it cannot write
        // its own admin dir, which leaves a half-removed worktree only a
        // prune can finish. Try that either way before reporting.
        if !root.exists() {
            let _ = git(repo, &["worktree", "prune"]).await;
        }
        return Ok(Reclaimed::Kept { reason: stderr });
    }

    let mut branch_deleted = false;
    if branch_exists(repo, branch).await? && commits_ahead(repo, branch).await == Some(0) {
        branch_deleted = git(repo, &["branch", "--delete", branch]).await.is_ok();
    }
    Ok(Reclaimed::Removed { branch_deleted })
}

/// How many commits `branch` has that the repository's own checkout does
/// not — what the issue actually produced.
pub async fn commits_ahead(repo: &Path, branch: &str) -> Option<usize> {
    let head = run(repo, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await
        .ok()?;
    let range = if head.status.success() {
        format!("HEAD..refs/heads/{branch}")
    } else if head.status.code() == Some(UNBORN_HEAD) {
        format!("refs/heads/{branch}")
    } else {
        return None;
    };
    let out = run(repo, &["rev-list", "--count", &range, "--"])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

const UNBORN_HEAD: i32 = 1;

async fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    Ok(run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await?
    .status
    .success())
}

async fn run(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .map_err(|e| ProjectError::Workdir(anyhow::anyhow!("spawn `git {}`: {e}", args.join(" "))))
}

async fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let out = run(repo, args).await?;
    if !out.status.success() {
        return Err(ProjectError::Workdir(anyhow::anyhow!(
            "`git {}` in {} failed: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "--quiet"])
            .await
            .expect("git init");
        dir
    }

    #[test]
    fn no_project_name_can_claim_the_worktrees_directory() {
        assert!(
            WORKTREES_DIR
                .chars()
                .any(|c| !c.is_ascii_alphanumeric() && c != '-'),
            "the worktrees directory must contain a character slugify cannot emit"
        );
        assert_eq!(crate::manager::slugify("Projects"), "projects");
    }

    #[test]
    fn a_branch_name_survives_a_title_that_slugifies_to_nothing() {
        assert_eq!(branch_name(7, "Wire the board"), "issue/7-wire-the-board");
        assert_eq!(branch_name(7, "!!!"), "issue/7");
        assert_eq!(branch_name(7, "???"), branch_name(7, "!!!"));
        assert!(branch_name(9, &"x".repeat(200)).len() < 60);
    }

    #[tokio::test]
    async fn a_project_with_no_commits_still_gets_a_worktree() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt-parent").join("1");
        let checkout = ensure(repo.path(), &root, "issue/1-first")
            .await
            .expect("worktree on a commit-less repo");
        assert_eq!(checkout.root, root);
        assert_eq!(checkout.repo, repo.path());
        assert!(root.join(".git").exists(), "worktree must be checked out");
    }

    #[tokio::test]
    async fn a_commit_made_in_the_worktree_lands_on_the_issue_branch() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("4");
        ensure(repo.path(), &root, "issue/4-thing")
            .await
            .expect("worktree");
        tokio::fs::write(root.join("a.txt"), b"hello")
            .await
            .expect("write a file the way a run would");
        commit(&root, "work").await;

        let head = run(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .expect("rev-parse");
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "issue/4-thing"
        );
        let log = run(repo.path(), &["log", "--oneline", "issue/4-thing"])
            .await
            .expect("log");
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("work"),
            "the commit must be visible on the branch from the main repo"
        );
    }

    #[tokio::test]
    async fn opening_the_same_checkout_twice_adopts_the_existing_tree() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("2");
        ensure(repo.path(), &root, "issue/2-x")
            .await
            .expect("first");
        tokio::fs::write(root.join("scratch.txt"), b"in progress")
            .await
            .expect("write");
        ensure(repo.path(), &root, "issue/2-x")
            .await
            .expect("second must adopt, not fail or reset");
        assert!(root.join("scratch.txt").exists());
    }

    async fn git_as_a_run(dir: &Path, work_dir: &Path, args: &[&str]) -> std::process::Output {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("HOME", work_dir)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .output()
            .await
            .expect("spawn git")
    }

    #[tokio::test]
    async fn a_run_can_commit_without_anyone_telling_it_who_it_is() {
        let repo = fresh_repo().await;
        let work_dir = tempfile::tempdir().expect("work dir");
        let root = repo.path().join("wt").join("5");
        ensure(repo.path(), &root, "issue/5-x")
            .await
            .expect("worktree");
        ensure_commit_identity(work_dir.path())
            .await
            .expect("identity");

        tokio::fs::write(root.join("a.txt"), b"work")
            .await
            .expect("write");
        let add = git_as_a_run(&root, work_dir.path(), &["add", "."]).await;
        assert!(add.status.success(), "git add must work");
        let commit = git_as_a_run(&root, work_dir.path(), &["commit", "-m", "done"]).await;
        assert!(
            commit.status.success(),
            "a run must be able to commit: {}",
            String::from_utf8_lossy(&commit.stderr)
        );

        let who = git_as_a_run(&root, work_dir.path(), &["log", "-1", "--format=%an <%ae>"]).await;
        assert_eq!(
            String::from_utf8_lossy(&who.stdout).trim(),
            "baybo <baybo@localhost>",
            "and the commit must be plainly machine-made"
        );
    }

    #[tokio::test]
    async fn an_identity_the_user_has_edited_is_left_alone() {
        let work_dir = tempfile::tempdir().expect("work dir");
        let file = work_dir.path().join(".gitconfig");
        tokio::fs::write(&file, b"[user]\n\tname = mine\n")
            .await
            .expect("write");
        ensure_commit_identity(work_dir.path())
            .await
            .expect("identity");
        assert_eq!(
            tokio::fs::read_to_string(&file).await.expect("read"),
            "[user]\n\tname = mine\n"
        );
    }

    #[tokio::test]
    async fn the_bootstrap_never_sweeps_up_work_the_user_had_staged() {
        let repo = fresh_repo().await;
        tokio::fs::write(repo.path().join("wip.txt"), b"secret")
            .await
            .expect("write");
        git(repo.path(), &["add", "wip.txt"])
            .await
            .expect("stage it");

        ensure(repo.path(), &repo.path().join("wt").join("1"), "issue/1-x")
            .await
            .expect("worktree");

        let log = run(repo.path(), &["log", "--all", "--oneline"])
            .await
            .expect("log");
        assert!(
            String::from_utf8_lossy(&log.stdout).trim().is_empty(),
            "opening a checkout must not commit anything: {}",
            String::from_utf8_lossy(&log.stdout)
        );
        let staged = run(repo.path(), &["diff", "--cached", "--name-only"])
            .await
            .expect("diff");
        assert_eq!(
            String::from_utf8_lossy(&staged.stdout).trim(),
            "wip.txt",
            "the user's index must be exactly as they left it"
        );
    }

    #[tokio::test]
    async fn a_worktree_whose_directory_vanished_can_be_cut_again() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("3");
        ensure(repo.path(), &root, "issue/3-x")
            .await
            .expect("first");
        tokio::fs::remove_dir_all(&root)
            .await
            .expect("lose the tree");

        ensure(repo.path(), &root, "issue/3-x")
            .await
            .expect("a lost worktree must be recoverable, not permanent");
        assert!(root.join(".git").exists());
    }

    async fn commit(dir: &Path, message: &str) {
        git(dir, &["add", "--all"]).await.expect("git add");
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@x",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        )
        .await
        .expect("git commit");
    }

    async fn repo_with_commit() -> tempfile::TempDir {
        let dir = fresh_repo().await;
        tokio::fs::write(dir.path().join("seed.txt"), b"seed")
            .await
            .expect("write");
        commit(dir.path(), "seed").await;
        dir
    }

    #[tokio::test]
    async fn reclaiming_a_clean_worktree_takes_its_commit_less_branch_with_it() {
        let repo = repo_with_commit().await;
        let root = repo.path().join("wt").join("6");
        ensure(repo.path(), &root, "issue/6-x").await.expect("cut");
        assert!(
            branch_exists(repo.path(), "issue/6-x")
                .await
                .expect("check"),
            "the branch is real once there is a commit to branch from"
        );

        assert_eq!(
            reclaim(repo.path(), &root, "issue/6-x")
                .await
                .expect("reclaim"),
            Reclaimed::Removed {
                branch_deleted: true
            },
            "a branch that produced nothing is not a deliverable"
        );
        assert!(!root.exists());
        assert!(
            !branch_exists(repo.path(), "issue/6-x")
                .await
                .expect("check")
        );
    }

    #[tokio::test]
    async fn an_orphan_worktree_leaves_no_branch_to_delete() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("6b");
        ensure(repo.path(), &root, "issue/6-x").await.expect("cut");
        assert!(
            !branch_exists(repo.path(), "issue/6-x")
                .await
                .expect("check")
        );
        assert_eq!(
            reclaim(repo.path(), &root, "issue/6-x")
                .await
                .expect("reclaim"),
            Reclaimed::Removed {
                branch_deleted: false
            }
        );
    }

    #[tokio::test]
    async fn a_branch_with_commits_outlives_its_worktree() {
        let repo = repo_with_commit().await;
        let root = repo.path().join("wt").join("7");
        ensure(repo.path(), &root, "issue/7-x").await.expect("cut");
        tokio::fs::write(root.join("a.txt"), b"work")
            .await
            .expect("write");
        commit(&root, "work").await;

        assert_eq!(
            reclaim(repo.path(), &root, "issue/7-x")
                .await
                .expect("reclaim"),
            Reclaimed::Removed {
                branch_deleted: false
            }
        );
        assert!(
            branch_exists(repo.path(), "issue/7-x")
                .await
                .expect("check")
        );
    }

    #[tokio::test]
    async fn a_commit_survives_reclaiming_a_card_on_a_project_with_no_commits_of_its_own() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("11");
        ensure(repo.path(), &root, "issue/11-x").await.expect("cut");
        tokio::fs::write(root.join("a.txt"), b"work")
            .await
            .expect("write");
        commit(&root, "work").await;

        assert_eq!(
            reclaim(repo.path(), &root, "issue/11-x")
                .await
                .expect("reclaim"),
            Reclaimed::Removed {
                branch_deleted: false
            }
        );
        assert!(
            branch_exists(repo.path(), "issue/11-x")
                .await
                .expect("check"),
            "the run's only deliverable must outlive the worktree"
        );
    }

    #[tokio::test]
    async fn every_commit_counts_when_the_repository_has_none_of_its_own() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("12");
        ensure(repo.path(), &root, "issue/12-x").await.expect("cut");
        tokio::fs::write(root.join("a.txt"), b"one")
            .await
            .expect("write");
        commit(&root, "one").await;
        assert_eq!(commits_ahead(repo.path(), "issue/12-x").await, Some(1));

        tokio::fs::write(root.join("b.txt"), b"two")
            .await
            .expect("write");
        commit(&root, "two").await;
        assert_eq!(commits_ahead(repo.path(), "issue/12-x").await, Some(2));
    }

    #[tokio::test]
    async fn a_count_git_cannot_produce_is_unknown_not_zero() {
        let not_a_repo = tempfile::tempdir().expect("tempdir");
        assert_eq!(commits_ahead(not_a_repo.path(), "issue/1-x").await, None);

        let repo = repo_with_commit().await;
        assert_eq!(
            commits_ahead(repo.path(), "issue/1-x").await,
            None,
            "a branch that is not there cannot be counted either"
        );
    }

    #[tokio::test]
    async fn a_branch_counts_only_what_it_added() {
        let repo = repo_with_commit().await;
        let root = repo.path().join("wt").join("13");
        ensure(repo.path(), &root, "issue/13-x").await.expect("cut");
        assert_eq!(
            commits_ahead(repo.path(), "issue/13-x").await,
            Some(0),
            "a fresh branch has produced nothing"
        );

        tokio::fs::write(root.join("a.txt"), b"work")
            .await
            .expect("write");
        commit(&root, "work").await;
        assert_eq!(commits_ahead(repo.path(), "issue/13-x").await, Some(1));
    }

    #[tokio::test]
    async fn uncommitted_work_is_never_destroyed_by_reclamation() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("8");
        ensure(repo.path(), &root, "issue/8-x").await.expect("cut");
        tokio::fs::write(root.join("half-done.txt"), b"in progress")
            .await
            .expect("write");

        let outcome = reclaim(repo.path(), &root, "issue/8-x")
            .await
            .expect("reclaim");
        assert!(
            matches!(&outcome, Reclaimed::Kept { reason } if reason.contains("untracked")),
            "the reason has to say why, because it lands on the timeline: {outcome:?}"
        );
        assert!(
            root.join("half-done.txt").exists(),
            "the work is still there"
        );
    }

    #[tokio::test]
    async fn reclaiming_twice_is_not_an_error() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("9");
        ensure(repo.path(), &root, "issue/9-x").await.expect("cut");
        reclaim(repo.path(), &root, "issue/9-x")
            .await
            .expect("first");
        assert_eq!(
            reclaim(repo.path(), &root, "issue/9-x")
                .await
                .expect("second"),
            Reclaimed::Absent
        );
    }

    #[tokio::test]
    async fn a_retitled_issue_keeps_the_branch_its_worktree_is_on() {
        let repo = repo_with_commit().await;
        let root = repo.path().join("wt").join("10");
        let first = branch_name(10, "Wire the board");
        ensure(repo.path(), &root, &first).await.expect("cut");

        let renamed = branch_name(10, "Wire the board properly");
        assert_ne!(renamed, first, "the rename does change what a guess says");
        assert_eq!(
            branch_of(&root).await.as_deref(),
            Some(first.as_str()),
            "but the tree still knows what it is really on"
        );

        ensure(repo.path(), &root, &renamed).await.expect("adopt");
        assert_eq!(branch_of(&root).await.as_deref(), Some(first.as_str()));
        assert!(!branch_exists(repo.path(), &renamed).await.expect("check"));
    }

    #[tokio::test]
    async fn a_workdir_that_is_not_a_repository_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = ensure(dir.path(), &dir.path().join("wt"), "issue/1-x")
            .await
            .expect_err("a non-repo workdir cannot host a worktree");
        assert!(
            err.to_string().contains("not a git repository"),
            "the error must say what is wrong: {err}"
        );
    }
}
