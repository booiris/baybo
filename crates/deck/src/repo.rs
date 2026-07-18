//! Local git version history for card bundles.
//!
//! `workspace/deck/` is a standalone git repo; every bundle mutation
//! (install / update / purge) auto-commits the touched card's directory so
//! the operator can `git log`, `git diff`, and roll a card back to a prior
//! revision with ordinary git — the concrete payoff of storing bundles as
//! plain files. Best-effort by design: a commit failure (no `git` on
//! `PATH`, detached HEAD, index-lock contention) is logged and never fails
//! the underlying deck operation, mirroring the identity-repo auto-commit
//! in the Edit tool and deck's tracing-based provenance.
//!
//! Soft delete and restore never touch files, so they never commit; purge
//! commits the directory's removal while git keeps every prior revision
//! reachable — a purged card's code is still recoverable via history.

use std::path::Path;
use std::process::ExitStatus;

use tokio::process::Command;

/// Commit identity for deck auto-commits — supplied per-invocation (via
/// `-c`) so a commit lands on a host with no global git identity set.
const GIT_AUTHOR_NAME: &str = "Baybo";
const GIT_AUTHOR_EMAIL: &str = "baybo@local";

/// Keeps the atomic-rename staging scratch (and the update-path `.old`
/// backups, which live under it) out of version history.
const GITIGNORE_BODY: &str = ".staging/\n";

/// Stage the card's directory (plus the repo `.gitignore`) and commit iff
/// anything changed. Returns the short SHA, or `None` when the working
/// tree already matched HEAD (e.g. a byte-identical re-install). The
/// `card_id` scopes the `git add` so a concurrent mutation of another card
/// can't be swept into this commit.
pub async fn commit_card(
    deck_root: &Path,
    card_id: &str,
    title: &str,
    event: &str,
    detail: &str,
) -> Result<Option<String>, String> {
    ensure_repo(deck_root).await?;

    run_git(deck_root, &[], &["add", "-A", "--", ".gitignore", card_id]).await?;

    // `git diff --cached --quiet` exits 0 when nothing is staged — an
    // identical re-install has no new revision to record, which is a
    // no-op, not a failure.
    if git_exit(deck_root, &["diff", "--cached", "--quiet"])
        .await?
        .success()
    {
        return Ok(None);
    }

    let message =
        format!("deck: {event} {title} ({card_id})\n\nEvent: {event}\nDetail: {detail}\n");
    run_git(
        deck_root,
        &[
            "-c",
            &format!("user.name={GIT_AUTHOR_NAME}"),
            "-c",
            &format!("user.email={GIT_AUTHOR_EMAIL}"),
        ],
        &["commit", "--no-verify", "--quiet", "-m", &message],
    )
    .await?;

    let sha = run_git(deck_root, &[], &["rev-parse", "--short", "HEAD"]).await?;
    Ok(Some(sha.trim().to_string()))
}

/// Ensure `deck_root` is a git repo with the staging `.gitignore` in
/// place. Idempotent — a no-op once `.git` exists (the `.gitignore` is
/// still re-created if an operator removed it).
async fn ensure_repo(deck_root: &Path) -> Result<(), String> {
    tokio::fs::create_dir_all(deck_root)
        .await
        .map_err(|e| format!("create deck root: {e}"))?;
    let gitignore = deck_root.join(".gitignore");
    if !gitignore.exists() {
        tokio::fs::write(&gitignore, GITIGNORE_BODY)
            .await
            .map_err(|e| format!("write .gitignore: {e}"))?;
    }
    if deck_root.join(".git").exists() {
        return Ok(());
    }
    let status = Command::new("git")
        .arg("init")
        .args(["--quiet", "-b", "main"])
        .arg(deck_root)
        .status()
        .await
        .map_err(|e| format!("spawn git init: {e}"))?;
    if !status.success() {
        return Err(format!("git init exited with {status}"));
    }
    Ok(())
}

async fn run_git(
    deck_root: &Path,
    config_overrides: &[&str],
    subcommand: &[&str],
) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(deck_root);
    for tok in config_overrides {
        cmd.arg(tok);
    }
    for tok in subcommand {
        cmd.arg(tok);
    }
    let output = cmd.output().await.map_err(|e| format!("spawn git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} exited with {}: {}",
            subcommand.first().copied().unwrap_or(""),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a git subcommand whose non-zero exit is a meaningful answer (not an
/// error) — `diff --cached --quiet` returns 1 when there are staged
/// changes.
async fn git_exit(deck_root: &Path, subcommand: &[&str]) -> Result<ExitStatus, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(deck_root);
    for tok in subcommand {
        cmd.arg(tok);
    }
    cmd.status().await.map_err(|e| format!("spawn git: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn log_oneline(deck_root: &Path, pathspec: &str) -> Vec<String> {
        let out = run_git(deck_root, &[], &["log", "--oneline", "--", pathspec])
            .await
            .unwrap();
        out.lines().map(str::to_string).collect()
    }

    fn write_card(deck_root: &Path, card_id: &str, service_body: &str) {
        let dir = deck_root.join(card_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("service.js"), service_body).unwrap();
    }

    #[tokio::test]
    async fn install_then_update_keeps_prior_revision_recoverable() {
        let tmp = tempfile::tempdir().unwrap();
        let deck = tmp.path().join("deck");

        write_card(&deck, "card-a", "v1");
        let first = commit_card(&deck, "card-a", "Quota", "install", "hash-1")
            .await
            .unwrap();
        assert!(first.is_some(), "install must record a revision");

        write_card(&deck, "card-a", "v2");
        let second = commit_card(&deck, "card-a", "Quota", "update", "hash-1 -> hash-2")
            .await
            .unwrap();
        assert!(second.is_some());

        // Two revisions for the card, newest first.
        let history = log_oneline(&deck, "card-a").await;
        assert_eq!(history.len(), 2, "want install + update: {history:?}");
        assert!(history[0].contains("update"));
        assert!(history[1].contains("install"));

        // The prior version is recoverable from history.
        let old = run_git(&deck, &[], &["show", "HEAD~1:card-a/service.js"])
            .await
            .unwrap();
        assert_eq!(old.trim(), "v1");
    }

    #[tokio::test]
    async fn identical_reinstall_records_no_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let deck = tmp.path().join("deck");

        write_card(&deck, "card-a", "v1");
        assert!(
            commit_card(&deck, "card-a", "Quota", "install", "hash-1")
                .await
                .unwrap()
                .is_some()
        );
        // No file change — nothing to commit.
        assert!(
            commit_card(&deck, "card-a", "Quota", "install", "hash-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn commit_is_scoped_to_one_card() {
        let tmp = tempfile::tempdir().unwrap();
        let deck = tmp.path().join("deck");

        write_card(&deck, "card-a", "a1");
        write_card(&deck, "card-b", "b1");
        // Committing card-a must not sweep in card-b's still-untracked dir.
        commit_card(&deck, "card-a", "A", "install", "ha")
            .await
            .unwrap();
        assert!(log_oneline(&deck, "card-a").await.len() == 1);
        assert!(
            log_oneline(&deck, "card-b").await.is_empty(),
            "card-b must remain untracked"
        );
    }

    #[tokio::test]
    async fn purge_commits_deletion_but_history_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let deck = tmp.path().join("deck");

        write_card(&deck, "card-a", "v1");
        commit_card(&deck, "card-a", "Quota", "install", "hash-1")
            .await
            .unwrap();

        std::fs::remove_dir_all(deck.join("card-a")).unwrap();
        let purge = commit_card(&deck, "card-a", "Quota", "purge", "hard")
            .await
            .unwrap();
        assert!(purge.is_some(), "purge must record the removal");

        // Deletion + install are both in history; the code is still
        // recoverable from the pre-purge revision.
        let history = log_oneline(&deck, "card-a").await;
        assert_eq!(history.len(), 2, "{history:?}");
        let recovered = run_git(&deck, &[], &["show", "HEAD~1:card-a/service.js"])
            .await
            .unwrap();
        assert_eq!(recovered.trim(), "v1");
    }

    #[tokio::test]
    async fn staging_scratch_is_never_versioned() {
        let tmp = tempfile::tempdir().unwrap();
        let deck = tmp.path().join("deck");
        std::fs::create_dir_all(deck.join(".staging").join("card-a.old")).unwrap();
        std::fs::write(deck.join(".staging").join("card-a.old").join("x"), "junk").unwrap();

        write_card(&deck, "card-a", "v1");
        commit_card(&deck, "card-a", "Quota", "install", "hash-1")
            .await
            .unwrap();

        let tracked = run_git(&deck, &[], &["ls-files"]).await.unwrap();
        assert!(
            !tracked.contains(".staging"),
            "staging scratch must be gitignored: {tracked}"
        );
        assert!(tracked.contains("card-a/service.js"));
    }
}
