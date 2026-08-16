//! Reclaiming regenerable build output from a checkout nobody is working
//! in.
//!
//! A card parked in Review keeps its worktree — that is deliberate, the
//! branch is still being looked at — but the build output under it is not
//! part of what anybody is looking at. On a real board two such cards held
//! 5.66G of `target/`, all of it reproducible by running the build again,
//! while [`crate::worktree::reclaim`] waited for a Done the operator was in
//! no hurry to give.
//!
//! Everything here decides **which** bytes may go. When they go is the
//! maintenance caller's business, and the only thing it may ask for is the
//! verb at the bottom of this file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use baybo_store::project::{IssueRow, IssueRunRow};
use chrono::{DateTime, Utc};

use crate::ProjectManager;

/// Directory names whose contents a build tool owns and can produce again.
///
/// An explicit list rather than "anything git ignores": a `.env`, a
/// downloaded fixture and a half-written scratch file are all ignored too,
/// and none of them comes back. Every name here is a tool's own convention
/// that no one puts source in. `dist/` and `build/` are deliberately
/// absent — those can be the very artefact a card was opened to produce.
const BUILD_ARTIFACT_DIRS: [&str; 4] = ["target", "node_modules", ".venv", "__pycache__"];

/// Below this, reclaiming costs the operator more attention than it frees.
const MIN_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// What one sweep freed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimedArtifacts {
    pub checkouts_swept: usize,
    pub dirs_removed: usize,
    pub bytes_freed: u64,
}

/// The only thing a maintenance caller may ask a board to do about disk.
///
/// A verb, not a store: the caller cannot enumerate cards, read runs, or
/// name a path of its own to delete. It says how idle is idle, and the
/// board answers with what it freed.
#[async_trait::async_trait]
pub trait BuildArtifacts: Send + Sync {
    async fn reclaim_idle_build_artifacts(&self, idle_for: Duration) -> ReclaimedArtifacts;
}

#[async_trait::async_trait]
impl BuildArtifacts for ProjectManager {
    async fn reclaim_idle_build_artifacts(&self, idle_for: Duration) -> ReclaimedArtifacts {
        sweep(self, idle_for).await
    }
}

/// Whether this card is between runs.
///
/// `Held`, `Queued` and `Running` all hold the issue's dedupe slot, and any
/// of them means something is about to work in this checkout — a queued run
/// is one `git worktree add` away from a build.
pub(crate) fn between_runs(runs: &[IssueRunRow]) -> bool {
    runs.iter().all(|run| run.settled_at.is_some())
}

/// When anything last happened *in this checkout*: the card's own
/// `updated_at`, or its newest run's settle, whichever is later.
///
/// Deliberately not [`crate::driver::already_asked`]'s notion of activity,
/// which filters coordination runs out. That one asks "has the card changed
/// since the lead looked"; this one asks "has a shell run in this tree", and
/// the lead's wakes get a checkout like every other run.
pub(crate) fn checkout_last_touched(issue: &IssueRow, runs: &[IssueRunRow]) -> DateTime<Utc> {
    runs.iter()
        .filter_map(|run| run.settled_at)
        .max()
        .map_or(issue.updated_at, |settled| settled.max(issue.updated_at))
}

async fn sweep(manager: &ProjectManager, idle_for: Duration) -> ReclaimedArtifacts {
    let mut freed = ReclaimedArtifacts::default();
    let Ok(idle_span) = chrono::Duration::from_std(idle_for) else {
        // An unrepresentable TTL fails closed: sweeping nothing is the
        // outcome an operator can recover from.
        tracing::warn!(?idle_for, "build-artifact sweep: unusable idle window");
        return freed;
    };
    let cutoff = Utc::now() - idle_span;
    let work_dir = manager.paths().work_dir();

    for (project, number) in crate::worktree::checkouts_on_disk(manager.paths()).await {
        let Ok(Some(issue)) = manager.store().get_issue(&project, number).await else {
            // A checkout with no card is not this sweep's to judge: it is
            // either a race with card creation or an orphan a person should
            // look at.
            continue;
        };
        let Ok(runs) = manager.store().list_runs(&issue.id).await else {
            continue;
        };
        if !between_runs(&runs) || checkout_last_touched(&issue, &runs) > cutoff {
            continue;
        }
        let root = crate::worktree::worktree_root(manager.paths(), &project, number);
        let Some(root) = admitted(&root, &work_dir) else {
            continue;
        };
        // The path is a real worktree and not merely a directory sitting
        // where one belongs.
        if crate::worktree::is_checkout(&root).await != Some(root.clone()) {
            continue;
        }
        freed.checkouts_swept += 1;
        for name in BUILD_ARTIFACT_DIRS {
            let dir = root.join(name);
            let Some(dir) = admitted(&dir, &root) else {
                continue;
            };
            if !crate::worktree::is_ignored(&root, name).await {
                continue;
            }
            let bytes = tree_bytes(&dir).await;
            if bytes < MIN_ARTIFACT_BYTES {
                continue;
            }
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {
                    freed.dirs_removed += 1;
                    freed.bytes_freed += bytes;
                    tracing::info!(
                        issue = number,
                        dir = %dir.display(),
                        bytes,
                        "reclaimed regenerable build output from an idle checkout"
                    );
                }
                Err(e) => {
                    tracing::warn!(issue = number, dir = %dir.display(), error = %e, "could not reclaim build output")
                }
            }
        }
    }
    freed
}

/// How much a directory holds, walked off the async runtime — a `target/`
/// is hundreds of thousands of files.
async fn tree_bytes(dir: &Path) -> u64 {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || baybo_workspace::walk::tree_stats(&dir))
        .await
        .ok()
        .and_then(|stats| stats.ok())
        .map_or(0, |stats| stats.bytes)
}

/// The path, canonicalised, if it is a real directory that is still inside
/// `within` after every symlink is resolved.
///
/// The containment check is the c957e790 rule, and it is asked here for the
/// same reason it is asked on every tool call: the agent that worked this
/// card can write inside `work/`, so it can replace any component of this
/// path with a link somewhere else between one sweep and the next. A
/// recursive delete that trusts the layout would follow it.
fn admitted(path: &Path, within: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(path).ok()?;
    if !resolved.is_dir() {
        return None;
    }
    let root = std::fs::canonicalize(within).ok()?;
    if !resolved.starts_with(&root) || resolved == root {
        tracing::warn!(
            path = %path.display(),
            resolved = %resolved.display(),
            "build-artifact sweep refused a path outside the tree it was sweeping"
        );
        return None;
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, IssueId, IssueRunId, ProjectId};
    use baybo_store::project::{IssuePriority, IssueStatus, RunStatus, RunTrigger};

    fn issue(updated_at: DateTime<Utc>) -> IssueRow {
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number: 1,
            title: "card".to_owned(),
            description: String::new(),
            attachments: Vec::new(),
            status: IssueStatus::Review,
            priority: IssuePriority::None,
            assignee: None,
            position: 0,
            blocked_reason: None,
            cancelled_at: None,
            parent_issue_id: None,
            stage: 0,
            source_key: None,
            branch: None,
            created_at: updated_at,
            updated_at,
        }
    }

    fn run(status: RunStatus, settled_at: Option<DateTime<Utc>>) -> IssueRunRow {
        IssueRunRow {
            id: IssueRunId::generate(),
            issue_id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number: 1,
            agent_id: AgentProfileId::parse("dev-1".to_owned()).expect("agent"),
            session_id: None,
            trigger: RunTrigger::Started,
            status,
            attempt: 1,
            resumes: 0,
            error: None,
            created_at: Utc::now(),
            started_at: None,
            settled_at,
        }
    }

    #[test]
    fn a_card_with_anything_owed_is_not_between_runs() {
        let now = Utc::now();
        assert!(between_runs(&[run(RunStatus::Done, Some(now))]));
        assert!(!between_runs(&[
            run(RunStatus::Done, Some(now)),
            run(RunStatus::Queued, None),
        ]));
        assert!(
            !between_runs(&[run(RunStatus::Held, None)]),
            "a run parked on budget is still a run this checkout owes"
        );
    }

    #[test]
    fn a_checkout_is_as_recent_as_the_later_of_its_card_and_its_runs() {
        let now = Utc::now();
        let card = issue(now - chrono::Duration::days(3));
        assert_eq!(
            checkout_last_touched(&card, &[run(RunStatus::Done, Some(now))]),
            now,
            "a run that settled after the last edit is what the tree remembers"
        );
        assert_eq!(
            checkout_last_touched(&card, &[]),
            card.updated_at,
            "and a card nothing ever ran on falls back to its own edit"
        );
    }

    #[test]
    fn a_path_that_escapes_the_tree_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("checkout");
        std::fs::create_dir_all(root.join("target")).expect("target");
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("node_modules")).expect("symlink");

        assert!(
            admitted(&root.join("target"), &root).is_some(),
            "a real directory inside the tree is admitted"
        );
        assert!(
            admitted(&root.join("node_modules"), &root).is_none(),
            "a link pointing out of the tree is refused, however it got there"
        );
        assert!(
            admitted(&root, &root).is_none(),
            "and the tree itself is never an artifact directory"
        );
    }
}
