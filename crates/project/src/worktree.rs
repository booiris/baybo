//! The checkout an issue's run works in.
//!
//! Every issue gets its own git worktree of the project's repository, so
//! two cards worked at the same time cannot see each other's edits. The
//! worktree is created lazily, at the first run, and is idempotent
//! afterwards: a second run of the same issue continues in the tree the
//! first one left behind.
//!
//! The thing to know about git worktrees before reading the rest: a
//! worktree keeps almost nothing of its own on disk. Its `.git` is a
//! *file* pointing at `<repo>/.git/worktrees/<name>`, and the index,
//! refs, objects and reflogs for its branch all live under the main
//! repository. A process that can write the worktree but not the
//! repository can read files and run `git status` — which even exits 0 —
//! and then dies at `index.lock` the moment it tries to commit. That is
//! why [`Checkout`] carries both paths and why both are handed to the
//! sandbox.

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
///
/// Deliberately keyed on the issue **number** alone, with no slug. The
/// spec sketched `<number>-<slug>`, but a title is editable and a slug
/// derived from it is therefore not stable for the life of the issue —
/// a retitle would either strand the worktree at its old path or force a
/// rename of a directory that a run may be sitting in. The branch keeps
/// the slug, because a branch name is a deliverable a human reads once
/// and never renames.
pub fn worktree_root(paths: &WorkspacePaths, project: &ProjectId, number: i64) -> PathBuf {
    paths
        .work_dir()
        .join(WORKTREES_DIR)
        .join(project.as_str())
        .join(number.to_string())
}

/// Where every project's worktrees live, as a child of the work dir.
///
/// The leading dot is load-bearing. A project created without a workdir
/// gets `work/<slugify(name)>`, and `slugify` emits only ASCII
/// alphanumerics and `-` — so a name that produced this directory would
/// collide with it, and "Projects" is not an exotic name for a project.
/// A dot is the one thing a slug cannot contain.
const WORKTREES_DIR: &str = ".worktrees";

/// `issue/<number>-<slug>`, the branch a run's commits land on.
///
/// Deterministic on purpose, and therefore not `manager::slugify`: that
/// one falls back to a random id for a title that slugifies to nothing,
/// which is right for a directory that must be unique and wrong here.
/// Every run recomputes this name and looks the branch up by it, so a
/// title of pure punctuation must yield `issue/<number>` every time
/// rather than a fresh branch per run.
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

/// Enough of the title to recognise the branch, short enough to type.
const MAX_SLUG_CHARS: usize = 40;

/// Give sandboxed git commands somebody to be.
///
/// Without this a run can edit its worktree and stage files and then dies
/// on `git commit` with "Please tell me who you are" — the plumbing all
/// works and the identity is simply absent. Git resolves the committer
/// from env, then the repository's config, then `$HOME/.gitconfig`, and
/// inside the sandbox `HOME` is the work directory (`resolve_env` in
/// `baybo-sandbox`), so a file here is the one place that reaches every
/// git command without touching the user's own repository.
///
/// Never overwritten. It is an ordinary git config file in a directory the
/// user can open, so once it exists it is theirs to edit.
///
/// The identity is baybo's rather than the running agent's. Per-agent
/// attribution wants `GIT_AUTHOR_*` in the child env, and the Bash tool's
/// env channel is also the exact-match redaction list for injected
/// secrets — putting an agent id through it would scrub that id out of
/// every command's output. Splitting those two uses is the follow-up; a
/// wrong-but-present committer beats a run that cannot commit.
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
///
/// Idempotent: an existing worktree at `root` is adopted as-is rather
/// than recreated, so a follow-up run continues where the last one
/// stopped and an interrupted run leaves nothing to repair.
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

    // A project created without a workdir is `git init`ed and has no
    // commits at all — the *default* shape, not an edge case. Git infers
    // `--orphan` there and cuts the worktree anyway, which is why nothing
    // here manufactures a root commit first: doing that would have run
    // `git commit` against the project's own index, and `--allow-empty`
    // only *permits* an empty commit — it does not force one, so any work
    // the user had staged would have been swept into a commit they did not
    // make, authored by us.
    //
    // Reuse the branch if it is already there — a reopened issue whose
    // worktree was reclaimed keeps its history rather than colliding.
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
        // The usual cause is an admin directory left behind by an earlier
        // attempt whose checkout is gone — a crash mid-add, or a `worktree
        // remove` that deleted the tree and then failed. Git refuses the
        // path until that record is pruned, which would otherwise wedge the
        // issue for good; prune and try once more so the retry button and
        // the boot sweep can actually recover it.
        git(repo, &["worktree", "prune"]).await?;
        git(repo, &args).await?;
    }
    Ok(checkout)
}

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
        // `materialise_workdir` puts an auto-created repo at
        // `work/<slugify(name)>`, so if a project name could slugify to the
        // worktrees directory the two would fight over one path — and
        // "Projects" is not an exotic name for a project.
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
        // Same title, same branch — a run that recomputes the name must
        // find the branch the last run made, not create another.
        assert_eq!(branch_name(7, "???"), branch_name(7, "!!!"));
        assert!(branch_name(9, &"x".repeat(200)).len() < 60);
    }

    #[tokio::test]
    async fn a_project_with_no_commits_still_gets_a_worktree() {
        // The default project shape: created without a workdir, so the
        // manager `git init`ed it and HEAD is unborn. `git worktree add`
        // cannot branch off that, so this is the case that would break
        // every first run if the bootstrap commit were missing.
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
        // The end-to-end claim: an agent editing files in its worktree and
        // committing produces history on the issue's own branch, and the
        // project's own branch is untouched.
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("4");
        ensure(repo.path(), &root, "issue/4-thing")
            .await
            .expect("worktree");
        tokio::fs::write(root.join("a.txt"), b"hello")
            .await
            .expect("write a file the way a run would");
        git(&root, &["add", "."]).await.expect("git add");
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@x",
                "commit",
                "--quiet",
                "-m",
                "work",
            ],
        )
        .await
        .expect("git commit");

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
        // A follow-up run continues where the last one stopped, so
        // uncommitted work from run 1 is still there for run 2.
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

    /// Run git the way the sandbox does: `HOME` is the work directory, and
    /// nothing inherited from the developer's own shell is allowed to
    /// supply an identity the product would not have.
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
        // The whole worktree layer is plumbing until this passes: an agent
        // edits files, stages them, and commits — with no identity of its
        // own anywhere, exactly as a run has none.
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
        // The repo has staged content and no commits — the shape where an
        // "--allow-empty" bootstrap commit would silently author the user's
        // work-in-progress as ours.
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
        // A crash between `git worktree add` and the run leaves git's admin
        // record pointing at a directory that is gone. Git refuses to reuse
        // the path until that is pruned, which would wedge the issue for
        // good — retry included.
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
