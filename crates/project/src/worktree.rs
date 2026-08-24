//! The checkout an issue's run works in.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::ProjectId;
use baybo_store::project::{IssueRow, ProjectStore};
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

/// Who this checkout's commits belong to, as a config file a sandboxed shell
/// can be pointed at with `GIT_CONFIG_GLOBAL`. `None` when git could not be
/// asked at all.
///
/// The question is answered **here**, on the host, and only the answer crosses
/// into the sandbox. Handing the operator's own `~/.gitconfig` across instead
/// looks equivalent and is not: the sandbox remaps `HOME`, so every `~` in that
/// file — an `includeIf "gitdir:~/work/**"` condition, an `[include] path` —
/// re-expands against the workspace and silently resolves to nothing, which
/// defeats exactly the per-repository identity it was written to select, while
/// everything else in the file (`core.hooksPath`, pagers, credential helpers)
/// comes along uninvited. Asking git here gets `includeIf`, XDG, system and
/// repo-local evaluated the way the operator sees them, and carries two
/// strings.
///
/// Written at **global** scope, so the repository's own `.git/config` — and
/// anything the run itself sets mid-flight — still outranks it, exactly as
/// outside. Nothing is cached: a re-resolve costs one `git config` against a
/// sandbox spawn that costs several times more, and the operator editing their
/// identity is obeyed by the next command.
pub async fn ensure_identity_config(checkout: &Checkout) -> Option<PathBuf> {
    let (name, email) = resolve_identity(&checkout.root).await?;
    let file = identity_config_path(&checkout.root);
    let body = format!(
        "# Written by baybo: the identity git resolved for this checkout on the host.\n\
         [user]\n\tname = {}\n\temail = {}\n",
        config_value(&name),
        config_value(&email)
    );
    if tokio::fs::read_to_string(&file).await.ok().as_deref() == Some(body.as_str()) {
        return Some(file);
    }
    let tmp = file.with_extension("gitconfig.tmp");
    tokio::fs::write(&tmp, &body).await.ok()?;
    // Rename rather than write in place: a concurrent run's `git` may be
    // reading this file, and half a config is an identity git will reject.
    tokio::fs::rename(&tmp, &file).await.ok()?;
    Some(file)
}

/// Sibling of the worktree rather than a file inside it, so the run's `git
/// status` stays clean; [`reclaim`] takes it away with the tree.
fn identity_config_path(root: &Path) -> PathBuf {
    root.with_extension("gitconfig")
}

/// What git — with the gateway's own environment, so the real `HOME` — says
/// this checkout commits as. A key the operator has not set anywhere falls
/// back to baybo's own name, because a run that cannot commit is worse than
/// one signed by an obvious placeholder.
async fn resolve_identity(root: &Path) -> Option<(String, String)> {
    let out = run(
        root,
        &["config", "-z", "--get-regexp", r"^user\.(name|email)$"],
    )
    .await
    .ok()?;
    Some(identity_or_fallback(&String::from_utf8_lossy(&out.stdout)))
}

/// `git config -z` emits NUL-separated `key\nvalue` records, and answers
/// nothing at all when a key is unset — which is why the fallback lives here
/// rather than in a `git config` default.
///
/// `--get-regexp` lists **every** value of a key, one per config layer, in the
/// order git read them (system, global, an `includeIf` where its directive
/// sat, local, worktree). Later assignment wins here for the same reason it
/// wins in git: last read is the effective one. Asking `--get` per key would
/// say the same thing in two subprocesses instead of one.
fn identity_or_fallback(raw: &str) -> (String, String) {
    const FALLBACK_NAME: &str = "baybo";
    const FALLBACK_EMAIL: &str = "baybo@localhost";
    let mut name = None;
    let mut email = None;
    for record in raw.split('\0') {
        match record.split_once('\n') {
            Some(("user.name", v)) => name = Some(v.to_owned()),
            Some(("user.email", v)) => email = Some(v.to_owned()),
            _ => {}
        }
    }
    (
        name.unwrap_or_else(|| FALLBACK_NAME.to_owned()),
        email.unwrap_or_else(|| FALLBACK_EMAIL.to_owned()),
    )
}

/// Quote a resolved value into git-config syntax. Git's own parser is what
/// reads this back, so `"` and `\` have to survive the round trip, and a
/// newline would otherwise end the entry and turn the rest into a directive.
fn config_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
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

/// Where a project's repository is on disk.
///
/// Its own port because its one consumer — binding an issue run's checkout
/// into the sandbox — needs exactly this one fact about a board. Handing it
/// a `ProjectStore` instead would hand it `create_issue`, `enqueue_run` and
/// every other board write, to answer a question about a directory.
#[async_trait::async_trait]
pub trait ProjectRepo: Send + Sync {
    /// `None` if the project is gone or unreadable. The distinction is not
    /// carried because the one caller does the same thing either way: it
    /// binds nothing.
    async fn workdir(&self, project: &ProjectId) -> Option<PathBuf>;
}

/// Open the checkout an issue's run is handed, cutting it on first use.
/// The four steps in order, so no caller has to know they are four.
pub async fn prepare_for_issue(
    store: &Arc<dyn ProjectStore>,
    paths: &WorkspacePaths,
    issue: &IssueRow,
) -> Result<PathBuf> {
    let project = store
        .get_project(&issue.project_id)
        .await?
        .ok_or_else(|| ProjectError::NoSuchProject(issue.project_id.clone()))?;
    let root = worktree_root(paths, &issue.project_id, issue.number);
    let branch = branch_name(issue.number, &issue.title);
    ensure(Path::new(&project.workdir), &root, &branch).await?;
    Ok(root)
}

/// Every checkout that exists on disk, as `(project, issue number)`.
///
/// The inverse of [`worktree_root`], and next to it so the two spellings of
/// the layout cannot drift. Read from the filesystem rather than derived
/// from the board because that is the question being asked — what is taking
/// up space — and because a card list would miss exactly the orphans worth
/// noticing.
///
/// Anything that is not a real directory is skipped, symlinks included: the
/// per-issue `<number>.gitconfig` files are siblings of the `<number>`
/// directories, and a link where a checkout belongs is not a checkout.
pub async fn checkouts_on_disk(paths: &WorkspacePaths) -> Vec<(ProjectId, i64)> {
    let mut found = Vec::new();
    let root = paths.work_dir().join(WORKTREES_DIR);
    let Ok(mut projects) = tokio::fs::read_dir(&root).await else {
        return found;
    };
    while let Ok(Some(project_entry)) = projects.next_entry().await {
        if !is_real_dir(&project_entry).await {
            continue;
        }
        let Some(project) = project_entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .and_then(|name| ProjectId::parse(name).ok())
        else {
            continue;
        };
        let Ok(mut cards) = tokio::fs::read_dir(project_entry.path()).await else {
            continue;
        };
        while let Ok(Some(card)) = cards.next_entry().await {
            if !is_real_dir(&card).await {
                continue;
            }
            if let Some(number) = card
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i64>().ok())
            {
                found.push((project.clone(), number));
            }
        }
    }
    found
}

async fn is_real_dir(entry: &tokio::fs::DirEntry) -> bool {
    // `symlink_metadata`, so a link to a directory reads as what it is.
    tokio::fs::symlink_metadata(entry.path())
        .await
        .is_ok_and(|meta| meta.is_dir())
}

/// The worktree this path is the top of, if it is one at all. `None` for a
/// directory that merely sits where a checkout belongs.
pub async fn is_checkout(root: &Path) -> Option<PathBuf> {
    let out = run(root, &["rev-parse", "--show-toplevel"]).await.ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    std::fs::canonicalize(top).ok()
}

/// Whether git itself ignores `name` inside this checkout — asked of git
/// rather than assumed from the name, so a repository that tracks a
/// directory the sweep would otherwise recognise keeps it.
pub async fn is_ignored(root: &Path, name: &str) -> bool {
    let Ok(out) = run(root, &["check-ignore", "--quiet", "--", name]).await else {
        return false;
    };
    out.status.success()
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

    let _ = tokio::fs::remove_file(identity_config_path(root)).await;

    let mut branch_deleted = false;
    if branch_exists(repo, branch).await? && commits_ahead(repo, branch).await == Some(0) {
        branch_deleted = git(repo, &["branch", "--delete", branch]).await.is_ok();
    }
    Ok(Reclaimed::Removed { branch_deleted })
}

/// What happened when a card's branch was offered to the repository's own
/// checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// The branch is in. `into` names the branch the repository's own
    /// checkout is on — reported rather than assumed, because a repository
    /// parked on something other than its trunk merges *there*, and a card
    /// that does not say so is a card nobody can audit.
    Landed {
        into: String,
        commit: String,
        commits: usize,
    },
    /// The checkout already had every commit on the branch. Not an error:
    /// a card merged twice, or one whose work reached the trunk some other
    /// way, is in the state the caller wanted.
    AlreadyThere { into: String },
    /// Nothing was merged, and why — a sentence written for the agent that
    /// asked, because it is the one that has to decide what to do next.
    /// `retryable` is true only for a lock another writer holds; a conflict
    /// and a dirty tree both need somebody to act first.
    Refused { reason: String, retryable: bool },
}

/// Offer an issue's branch to the repository's own checkout.
///
/// Runs `git -C <repo> merge`, never `checkout`: the trunk is checked out in
/// the primary worktree, so a sibling checkout cannot take it — `git` refuses
/// `checkout`, `push . HEAD:<trunk>` and `branch -f` alike. Merging from
/// outside is the one shape that works, and it leaves the primary tree clean.
///
/// Every refusal is checked **before** the merge runs, because the failure
/// this exists to prevent is a half-merged trunk in the operator's own
/// working tree.
pub async fn merge(repo: &Path, root: &Path, branch: &str, message: &str) -> Result<Merged> {
    if !branch_exists(repo, branch).await? {
        return Ok(Merged::Refused {
            reason: format!("there is no branch `{branch}` in this repository"),
            retryable: false,
        });
    }
    // `branch_of` says nothing for a detached HEAD and nothing for a branch
    // that has never been committed to, and neither can be merged into:
    // measured on git 2.54, `merge --no-ff` into an empty head is refused by
    // git itself ("does not make sense into an empty head").
    let Some(into) = branch_of(repo).await else {
        return Ok(Merged::Refused {
            reason: "the repository's own checkout has no branch with commits on it to merge \
                     into — it is on a detached HEAD, or nothing has ever been committed to it. \
                     Either one needs a person."
                .to_owned(),
            retryable: false,
        });
    };
    if into == branch {
        return Ok(Merged::Refused {
            reason: format!(
                "the repository's own checkout is on `{branch}` itself, so there is nothing to \
                 merge it into"
            ),
            retryable: false,
        });
    }
    let commits = match commits_ahead(repo, branch).await {
        None => {
            return Ok(Merged::Refused {
                reason: format!("git could not count what `{branch}` carries"),
                retryable: false,
            });
        }
        Some(0) => return Ok(Merged::AlreadyThere { into }),
        Some(n) => n,
    };
    if let Some(dirty) = dirty_tracked(repo).await {
        return Ok(Merged::Refused {
            reason: format!(
                "the repository's own checkout has uncommitted changes, and merging into it \
                 would mix them with this branch: {dirty}"
            ),
            retryable: false,
        });
    }
    // The card's own checkout is checked too, and this is the refusal worth
    // having: `git -C <repo> merge` never looks at it, so work still sitting
    // uncommitted in the worktree is silently *absent* from what lands, and
    // the agent would read a success it did not get.
    if root.exists()
        && let Some(dirty) = dirty_tracked(root).await
    {
        return Ok(Merged::Refused {
            reason: format!(
                "this issue's checkout has uncommitted changes, which a merge would leave \
                 behind: {dirty}. Commit them to `{branch}` first."
            ),
            retryable: false,
        });
    }

    // A merge commit needs a committer, and this one is made on the host,
    // outside the sandbox — so it reaches neither the per-checkout config
    // `ensure_identity_config` writes nor `BashTool`'s `git()` shim. Without
    // this a host that has never had `user.email` set answers "Committer
    // identity unknown", which reaches the agent as a refusal it cannot act
    // on and which reads like a conflict. Resolved the same way, and through
    // the same fallback, as the config handed to a sandboxed run.
    let (name, email) = resolve_identity(repo)
        .await
        .unwrap_or_else(|| identity_or_fallback(""));
    let user_name = format!("user.name={name}");
    let user_email = format!("user.email={email}");
    let out = run(
        repo,
        &[
            "-c",
            &user_name,
            "-c",
            &user_email,
            "merge",
            "--no-ff",
            "-m",
            message,
            branch,
        ],
    )
    .await?;
    if !out.status.success() {
        // Conflict text goes to **stdout**, not stderr, so a reader that
        // takes only one of the two reports a failure with no reason in it.
        let mut said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.trim().is_empty() {
            if !said.is_empty() {
                said.push('\n');
            }
            said.push_str(stderr.trim());
        }
        // A lock is another writer, not a disagreement about the code: the
        // same call a moment later is the right answer, and telling an agent
        // to resolve a conflict that does not exist sends it to rewrite code
        // nobody objected to.
        let retryable = said.contains(LOCK_MARKER);
        if !retryable {
            let _ = run(repo, &["merge", "--abort"]).await;
        }
        return Ok(Merged::Refused {
            reason: said,
            retryable,
        });
    }
    let commit = run(repo, &["rev-parse", "HEAD"])
        .await
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    Ok(Merged::Landed {
        into,
        commit,
        commits,
    })
}

/// `.git/index.lock` is git's own word for "another writer is in here".
const LOCK_MARKER: &str = "index.lock";

/// The tracked files this checkout has changed, as one line, or `None` when
/// it is clean.
///
/// `--untracked-files=no` is not an optimisation: a real repository is full
/// of build output, and a plain `--porcelain` would report `?? target/` and
/// refuse every merge there has ever been.
async fn dirty_tracked(root: &Path) -> Option<String> {
    let out = run(root, &["status", "--porcelain", "--untracked-files=no"])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let said = String::from_utf8_lossy(&out.stdout);
    let paths: Vec<&str> = said
        .lines()
        .filter_map(|line| line.get(3..))
        .take(MAX_DIRTY_PATHS)
        .collect();
    (!paths.is_empty()).then(|| paths.join(", "))
}

/// Enough to recognise what is in the way without pasting a whole tree into
/// a tool answer.
const MAX_DIRTY_PATHS: usize = 5;

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

    /// A repository with a commit on its own checkout, plus an issue
    /// worktree carrying one commit of its own.
    async fn repo_with_a_branch_to_land() -> (tempfile::TempDir, PathBuf) {
        let repo = fresh_repo().await;
        tokio::fs::write(repo.path().join("base.txt"), b"base")
            .await
            .expect("seed");
        commit(repo.path(), "base").await;
        let root = repo.path().join("wt").join("5");
        ensure(repo.path(), &root, "issue/5-work")
            .await
            .expect("worktree");
        tokio::fs::write(root.join("done.txt"), b"work")
            .await
            .expect("work");
        commit(&root, "the work").await;
        (repo, root)
    }

    #[tokio::test]
    async fn a_branch_lands_on_whatever_the_repository_is_parked_on_and_says_which() {
        let (repo, root) = repo_with_a_branch_to_land().await;
        let merged = merge(repo.path(), &root, "issue/5-work", "Merge #5: the work")
            .await
            .expect("merge");
        let Merged::Landed {
            into,
            commit,
            commits,
        } = merged
        else {
            panic!("expected a landing, got {merged:?}");
        };
        assert_eq!(
            into, "master",
            "the trunk it landed on is reported, not assumed"
        );
        assert_eq!(commits, 1);
        assert!(!commit.is_empty(), "the merge commit has to be nameable");

        // The repository's own tree is left usable, which is the whole
        // reason this merges from outside rather than checking the trunk out.
        // Tracked files only: this fixture keeps its worktree *inside* the
        // repo, which production does not, and `?? wt/` is not dirt.
        let status = run(
            repo.path(),
            &["status", "--porcelain", "--untracked-files=no"],
        )
        .await
        .expect("status");
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "the primary checkout must be clean after a merge"
        );
        assert_eq!(commits_ahead(repo.path(), "issue/5-work").await, Some(0));
    }

    /// The merge is made on the host, outside the sandbox, so it reaches
    /// neither shim that gives a sandboxed commit its identity. Git refuses
    /// to make a merge commit without one, so the identity has to be
    /// resolved and passed here or the whole verb breaks on a bare host.
    #[tokio::test]
    async fn the_merge_commit_carries_an_identity_git_will_accept() {
        let (repo, root) = repo_with_a_branch_to_land().await;
        git(
            repo.path(),
            &["config", "user.email", "board@example.invalid"],
        )
        .await
        .expect("local identity");
        git(repo.path(), &["config", "user.name", "The Board"])
            .await
            .expect("local identity");

        let merged = merge(repo.path(), &root, "issue/5-work", "Merge #5: the work")
            .await
            .expect("merge");
        assert!(matches!(merged, Merged::Landed { .. }), "{merged:?}");

        let who = run(repo.path(), &["log", "-1", "--format=%cn <%ce>"])
            .await
            .expect("log");
        assert_eq!(
            String::from_utf8_lossy(&who.stdout).trim(),
            "The Board <board@example.invalid>",
            "the identity the repository resolves is the one the merge commits as"
        );
    }

    #[tokio::test]
    async fn merging_the_same_branch_twice_is_not_an_error() {
        let (repo, root) = repo_with_a_branch_to_land().await;
        merge(repo.path(), &root, "issue/5-work", "Merge #5: the work")
            .await
            .expect("first");
        let again = merge(repo.path(), &root, "issue/5-work", "Merge #5: the work")
            .await
            .expect("second");
        assert!(
            matches!(again, Merged::AlreadyThere { .. }),
            "a card whose branch is already in is in the state the caller wanted: {again:?}"
        );
    }

    /// The refusal that only this layer can make: `git -C <repo> merge` never
    /// looks at the card's own checkout, so uncommitted work there would be
    /// absent from what lands while the agent read a success.
    #[tokio::test]
    async fn uncommitted_work_in_the_card_s_own_checkout_refuses_the_merge() {
        let (repo, root) = repo_with_a_branch_to_land().await;
        tokio::fs::write(root.join("done.txt"), b"work, and more since")
            .await
            .expect("dirty the worktree");

        let merged = merge(repo.path(), &root, "issue/5-work", "Merge #5: the work")
            .await
            .expect("merge");
        let Merged::Refused { reason, retryable } = merged else {
            panic!("a dirty card checkout must refuse, got {merged:?}");
        };
        assert!(reason.contains("done.txt"), "{reason}");
        assert!(!retryable, "committing is somebody's job, not a retry");
        assert_eq!(
            commits_ahead(repo.path(), "issue/5-work").await,
            Some(1),
            "nothing may have landed"
        );
    }

    #[tokio::test]
    async fn uncommitted_work_in_the_repository_s_own_checkout_refuses_the_merge() {
        let (repo, root) = repo_with_a_branch_to_land().await;
        tokio::fs::write(repo.path().join("base.txt"), b"the operator was mid-edit")
            .await
            .expect("dirty the primary tree");

        let merged = merge(repo.path(), &root, "issue/5-work", "Merge #5: the work")
            .await
            .expect("merge");
        assert!(
            matches!(&merged, Merged::Refused { reason, .. } if reason.contains("base.txt")),
            "the operator's own uncommitted work must stop the merge: {merged:?}"
        );
    }

    /// Build output is what an untracked file is in a real repository, and a
    /// merge that refused on those would never run anywhere.
    #[tokio::test]
    async fn an_untracked_file_does_not_stop_a_merge() {
        let (repo, root) = repo_with_a_branch_to_land().await;
        tokio::fs::write(repo.path().join("target-ish.log"), b"build output")
            .await
            .expect("untracked");
        let merged = merge(repo.path(), &root, "issue/5-work", "Merge #5: the work")
            .await
            .expect("merge");
        assert!(matches!(merged, Merged::Landed { .. }), "{merged:?}");
    }

    #[tokio::test]
    async fn a_conflict_is_reported_and_leaves_no_merge_half_done() {
        let repo = fresh_repo().await;
        tokio::fs::write(repo.path().join("both.txt"), b"base")
            .await
            .expect("seed");
        commit(repo.path(), "base").await;
        let root = repo.path().join("wt").join("6");
        ensure(repo.path(), &root, "issue/6-theirs")
            .await
            .expect("worktree");
        tokio::fs::write(root.join("both.txt"), b"theirs")
            .await
            .expect("their edit");
        commit(&root, "theirs").await;
        // And the trunk moves under it, on the same line.
        tokio::fs::write(repo.path().join("both.txt"), b"ours")
            .await
            .expect("our edit");
        commit(repo.path(), "ours").await;

        let merged = merge(repo.path(), &root, "issue/6-theirs", "Merge #6: theirs")
            .await
            .expect("merge");
        let Merged::Refused { reason, retryable } = merged else {
            panic!("a conflict must refuse, got {merged:?}");
        };
        // Conflict text is on stdout, and a refusal with no reason in it is
        // one the agent cannot act on.
        assert!(reason.to_lowercase().contains("conflict"), "{reason}");
        assert!(!retryable, "a conflict needs somebody to resolve it");
        let status = run(
            repo.path(),
            &["status", "--porcelain", "--untracked-files=no"],
        )
        .await
        .expect("status");
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "an aborted merge must leave the operator's checkout as it was"
        );
    }

    #[tokio::test]
    async fn a_branch_that_is_not_there_refuses_rather_than_failing() {
        let repo = fresh_repo().await;
        tokio::fs::write(repo.path().join("base.txt"), b"base")
            .await
            .expect("seed");
        commit(repo.path(), "base").await;
        let merged = merge(
            repo.path(),
            &repo.path().join("nowhere"),
            "issue/99-ghost",
            "Merge #99",
        )
        .await
        .expect("merge");
        assert!(
            matches!(&merged, Merged::Refused { reason, .. } if reason.contains("issue/99-ghost")),
            "{merged:?}"
        );
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

    /// A run's shell as the sandbox actually shapes it: `HOME` remapped away
    /// from the operator's own (`baybo_sandbox`'s `resolve_env` points it at
    /// the workspace), no `GIT_AUTHOR_*`/`GIT_COMMITTER_*`, and the identity
    /// reaching it only through `GIT_CONFIG_GLOBAL`. The remap is the whole
    /// point: a test that leaves `HOME` on the operator's home proves nothing,
    /// because that is the one thing production takes away.
    async fn git_in_sandbox(
        dir: &Path,
        home: &Path,
        config: &Path,
        args: &[&str],
    ) -> std::process::Output {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", config)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .output()
            .await
            .expect("spawn git")
    }

    #[tokio::test]
    async fn a_run_commits_as_whoever_the_repository_says_the_operator_is() {
        let repo = fresh_repo().await;
        // Repo-local, so the assertion is about this checkout rather than
        // about whichever identity the host running the suite happens to have.
        git(repo.path(), &["config", "user.name", "Ada Lovelace"])
            .await
            .expect("name");
        git(repo.path(), &["config", "user.email", "ada@example.com"])
            .await
            .expect("email");
        let root = repo.path().join("wt").join("5");
        ensure(repo.path(), &root, "issue/5-x")
            .await
            .expect("worktree");
        let config = ensure_identity_config(&Checkout {
            root: root.clone(),
            repo: repo.path().to_path_buf(),
        })
        .await
        .expect("identity config");
        let home = tempfile::tempdir().expect("remapped home");

        tokio::fs::write(root.join("a.txt"), b"work")
            .await
            .expect("write");
        let add = git_in_sandbox(&root, home.path(), &config, &["add", "."]).await;
        assert!(add.status.success(), "git add must work");
        let commit = git_in_sandbox(&root, home.path(), &config, &["commit", "-m", "done"]).await;
        assert!(
            commit.status.success(),
            "a run must be able to commit: {}",
            String::from_utf8_lossy(&commit.stderr)
        );

        let who = git_in_sandbox(
            &root,
            home.path(),
            &config,
            &["log", "-1", "--format=%an <%ae>"],
        )
        .await;
        assert_eq!(
            String::from_utf8_lossy(&who.stdout).trim(),
            "Ada Lovelace <ada@example.com>",
            "the operator's identity has to survive the HOME remap"
        );
    }

    #[tokio::test]
    async fn reclaiming_a_checkout_takes_its_identity_config_with_it() {
        let repo = fresh_repo().await;
        let root = repo.path().join("wt").join("6");
        ensure(repo.path(), &root, "issue/6-x")
            .await
            .expect("worktree");
        let config = ensure_identity_config(&Checkout {
            root: root.clone(),
            repo: repo.path().to_path_buf(),
        })
        .await
        .expect("identity config");
        assert!(config.exists());

        reclaim(repo.path(), &root, "issue/6-x")
            .await
            .expect("reclaim");
        assert!(
            !config.exists(),
            "a reclaimed tree leaves no identity behind"
        );
    }

    #[test]
    fn the_last_layer_that_set_a_key_is_the_one_that_answers() {
        // What a repo-local override looks like on the wire: the global value
        // first, the winning one second.
        assert_eq!(
            identity_or_fallback("user.email\nglobal@example.com\0user.email\nlocal@example.com\0"),
            ("baybo".to_owned(), "local@example.com".to_owned())
        );
    }

    #[test]
    fn an_unset_key_falls_back_rather_than_leaving_the_run_unable_to_commit() {
        assert_eq!(
            identity_or_fallback("user.name\nAda\0user.email\nada@example.com\0"),
            ("Ada".to_owned(), "ada@example.com".to_owned())
        );
        assert_eq!(
            identity_or_fallback("user.name\nAda\0"),
            ("Ada".to_owned(), "baybo@localhost".to_owned())
        );
        assert_eq!(
            identity_or_fallback(""),
            ("baybo".to_owned(), "baybo@localhost".to_owned())
        );
    }

    #[test]
    fn a_value_cannot_break_out_of_the_entry_it_is_written_into() {
        // Unquoted, a `"` or a newline would end the value and turn whatever
        // follows into config syntax of the operator's choosing.
        assert_eq!(config_value(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(config_value("two\nlines"), r#""two lines""#);
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
