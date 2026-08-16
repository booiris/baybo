mod error;
mod fs_sweep;
mod sidecar_sweep;

pub use error::JanitorError;

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baybo_store::ChannelPairingStore;
use baybo_workspace::WorkspacePaths;

use fs_sweep::{DirSweep, is_log_file, sweep_directory, sweep_tree_entries};

const LOG_FILE_TTL: Duration = Duration::from_secs(30 * 86_400);
// Disposable-scratch window for `<workspace>/work/tmp`. The day count
// lives in `baybo-workspace` because the Bash tool description
// advertises the same figure to the model.
const WORK_TMP_TTL: Duration =
    Duration::from_secs(baybo_workspace::paths::WORK_TMP_TTL_DAYS * 86_400);
// Pairing approvals — short-lived auth-flow ephemera, kept long enough
// for the next channel reload to confirm them, then dropped.
const PAIRING_APPROVAL_TTL: Duration = Duration::from_secs(7 * 86_400);
// `channel_pairings` runs hourly (much faster than the daily
// sweep) because pending codes expire on the order of minutes; an
// hourly cadence keeps the table from accumulating expired pending
// rows between full sweeps.
const PAIRING_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
// Stale-sidecar-bundle window. The cache root is shared across every
// Baybo process running under the same UID, so the TTL also doubles as
// a safety margin: a concurrent older-version Baybo that's actively
// using one of the dirs touches it (spawn child reads bundle.mjs, the
// browser sidecar opens the docker/ aux dir on every `docker build`)
// and will look fresh enough to skip. Operators running multiple Baybo
// versions side-by-side for >7 days without restarting either is well
// outside the realistic case.
const SIDECAR_CACHE_TTL: Duration = Duration::from_secs(7 * 86_400);
// Tool-output spills are content-addressed transcripts of results too big
// to inline; the model reads one during the turn that produced it and
// essentially never after. The same window as `work/tmp` because they are
// the same kind of thing — scratch the agent left behind.
const TOOL_SPILL_TTL: Duration = WORK_TMP_TTL;
// How long an issue checkout must go untouched before its build output is
// considered regenerable rather than in use. Longer than the scratch
// windows above by an order of magnitude: the cost of being wrong is a
// rebuild on the next run, but a card picked up again after a weekend
// should still find its cache warm.
const IDLE_CHECKOUT_TTL: Duration = Duration::from_secs(3 * 86_400);
const TICK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
// Sidecar-cache cleanup is much rarer than the other sweeps — it only
// reclaims after a binary upgrade lands a fresh content hash, and the
// total cruft per upgrade is single-digit MB. Daily cadence keeps the
// walk off the hot path while still bounding the eventual size.
const SIDECAR_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default, Clone, Copy)]
pub struct JanitorReport {
    pub log_files_removed: usize,
    pub work_tmp_removed: usize,
    pub tool_spills_removed: usize,
    pub sidecar_dirs_removed: usize,
    pub pairings_purged: u64,
    pub build_dirs_removed: usize,
    pub build_bytes_freed: u64,
}

/// What one build-artifact sweep freed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimedArtifacts {
    pub dirs_removed: usize,
    pub bytes_freed: u64,
}

/// Regenerable build output somebody else owns the rules for.
///
/// The janitor knows a cadence and a TTL and nothing else: which checkouts
/// exist, whether one is between runs, and what inside it a build tool can
/// make again are all questions for whoever owns those trees. This is the
/// whole surface of that arrangement — a verb the janitor may call, never a
/// store it may read.
#[async_trait::async_trait]
pub trait BuildArtifactSource: Send + Sync {
    async fn reclaim_idle(&self, idle_for: Duration) -> ReclaimedArtifacts;
}

/// Live-set view consumed by the sidecar-cache sweep. `cache_root` is
/// `$XDG_CACHE_HOME/baybo/sidecars/`; `live_dirs` is the
/// `<name>-<hash>` set the running Baybo currently has materialised.
/// Anything else under `cache_root` whose mtime is older than the TTL
/// is left over from a previous Baybo version and gets removed.
#[derive(Debug, Clone)]
pub struct SidecarCache {
    pub cache_root: PathBuf,
    pub live_dirs: HashSet<String>,
}

pub struct Janitor {
    paths: WorkspacePaths,
    sidecar_cache: Option<SidecarCache>,
    pairings: Option<Arc<dyn ChannelPairingStore>>,
    build_artifacts: Option<Arc<dyn BuildArtifactSource>>,
}

impl Janitor {
    pub fn new(paths: WorkspacePaths) -> Self {
        Self {
            paths,
            sidecar_cache: None,
            pairings: None,
            build_artifacts: None,
        }
    }

    /// Enable the sidecar-cache sweep. Call before [`Self::run`]; the
    /// constructor leaves it disabled so unit tests / integration
    /// callers that don't have a `SidecarRuntime` can still build a
    /// Janitor.
    #[must_use]
    pub fn with_sidecar_cache(mut self, cache: SidecarCache) -> Self {
        self.sidecar_cache = Some(cache);
        self
    }

    /// Wire the pairing store for the hourly expired-code sweep.
    /// Without this call the pairing sweep doesn't run.
    #[must_use]
    pub fn with_pairing_store(mut self, pairings: Arc<dyn ChannelPairingStore>) -> Self {
        self.pairings = Some(pairings);
        self
    }

    /// Wire the build-artifact sweep. Without this call it does not run —
    /// a workspace with no boards has no checkouts to sweep.
    #[must_use]
    pub fn with_build_artifacts(mut self, source: Arc<dyn BuildArtifactSource>) -> Self {
        self.build_artifacts = Some(source);
        self
    }

    /// Run every wired sweep once. Failures in one sweep are logged and
    /// the others still run — janitor is best-effort.
    pub async fn sweep_once(&self) -> JanitorReport {
        let mut report = JanitorReport::default();

        let mut logs_total = 0;
        for dir in [self.paths.logs_dir(), self.paths.channel_logs_dir()] {
            match sweep_directory(DirSweep {
                dir: &dir,
                ttl: LOG_FILE_TTL,
                name_predicate: is_log_file,
            })
            .await
            {
                Ok(n) => logs_total += n,
                Err(e) => tracing::warn!(error = %e, dir = %dir.display(), "log sweep failed"),
            }
        }
        report.log_files_removed = logs_total;

        let scratch = self.paths.work_tmp_dir();
        match sweep_tree_entries(&scratch, WORK_TMP_TTL).await {
            Ok(n) => report.work_tmp_removed = n,
            Err(e) => {
                tracing::warn!(error = %e, dir = %scratch.display(), "work/tmp sweep failed")
            }
        }

        let spills = self.paths.tool_spills_dir();
        match sweep_tree_entries(&spills, TOOL_SPILL_TTL).await {
            Ok(n) => report.tool_spills_removed = n,
            Err(e) => {
                tracing::warn!(error = %e, dir = %spills.display(), "tool-spill sweep failed")
            }
        }

        if let Some(source) = self.build_artifacts.as_ref() {
            let freed = source.reclaim_idle(IDLE_CHECKOUT_TTL).await;
            report.build_dirs_removed = freed.dirs_removed;
            report.build_bytes_freed = freed.bytes_freed;
        }

        // Pairings get a daily sweep here too so a long-running process
        // that never trips the hourly tick (e.g. heavy load deferring
        // every interval fire) still eventually reaps stale rows.
        if self.pairings.is_some() {
            report.pairings_purged += self.sweep_pairings_once(chrono::Utc::now()).await;
        }

        if report.log_files_removed > 0
            || report.work_tmp_removed > 0
            || report.tool_spills_removed > 0
            || report.pairings_purged > 0
            || report.build_dirs_removed > 0
        {
            tracing::info!(
                log_files_removed = report.log_files_removed,
                work_tmp_removed = report.work_tmp_removed,
                tool_spills_removed = report.tool_spills_removed,
                pairings_purged = report.pairings_purged,
                build_dirs_removed = report.build_dirs_removed,
                build_bytes_freed = report.build_bytes_freed,
                "janitor sweep complete",
            );
        } else {
            tracing::debug!(
                log_files_removed = report.log_files_removed,
                work_tmp_removed = report.work_tmp_removed,
                tool_spills_removed = report.tool_spills_removed,
                pairings_purged = report.pairings_purged,
                "janitor sweep complete",
            );
        }

        report
    }

    /// Hourly pairing sweep. Returns the number of rows hard-deleted.
    /// Pending rows expire on the order of minutes; without a
    /// faster-than-daily cadence the table fills with dead pending
    /// codes between the main sweep ticks.
    pub async fn sweep_pairings_once(&self, now: chrono::DateTime<chrono::Utc>) -> u64 {
        let Some(pairings) = self.pairings.as_ref() else {
            return 0;
        };
        let approved_cutoff =
            (now - chrono::Duration::seconds(PAIRING_APPROVAL_TTL.as_secs() as i64)).timestamp();
        match pairings
            .purge_expired(now.timestamp(), approved_cutoff)
            .await
        {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "pairing sweep failed");
                0
            }
        }
    }

    /// Background loop: sweep at boot, then every [`TICK_INTERVAL`]
    /// (12h). The sidecar-cache sweep is gated separately at
    /// [`SIDECAR_SWEEP_INTERVAL`] (24h) so it runs every other tick
    /// rather than on the same cadence as the day-scoped sweeps.
    /// Returns when `shutdown` resolves.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        // First tick fires immediately; subsequent ticks delay rather
        // than burst-catch-up so a slow sweep can't stack.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut pairing_interval = tokio::time::interval(PAIRING_SWEEP_INTERVAL);
        pairing_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::debug!(tick_secs = TICK_INTERVAL.as_secs(), "janitor started",);
        // First-tick sentinel: sweep immediately on boot, then space
        // subsequent runs by `SIDECAR_SWEEP_INTERVAL`.
        let mut last_sidecar_sweep: Option<Instant> = None;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let _ = self.sweep_once().await;
                    let due = last_sidecar_sweep
                        .map(|t| t.elapsed() >= SIDECAR_SWEEP_INTERVAL)
                        .unwrap_or(true);
                    if due {
                        let _ = self.sweep_sidecar_cache().await;
                        last_sidecar_sweep = Some(Instant::now());
                    }
                }
                _ = pairing_interval.tick() => {
                    let _ = self.sweep_pairings_once(chrono::Utc::now()).await;
                }
                _ = &mut shutdown => {
                    tracing::debug!("janitor shutting down");
                    break;
                }
            }
        }
    }

    /// One pass of the sidecar-cache sweep. Public for tests; the run
    /// loop calls this on its own daily cadence.
    pub async fn sweep_sidecar_cache(&self) -> usize {
        let Some(cache) = self.sidecar_cache.as_ref() else {
            return 0;
        };
        match sidecar_sweep::sweep(cache, SIDECAR_CACHE_TTL).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(removed = n, "sidecar-cache sweep complete");
                }
                n
            }
            Err(e) => {
                tracing::warn!(error = %e, "sidecar-cache sweep failed");
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::SystemTime;

    use baybo_workspace::test_support::{back_date, back_date_symlink, back_date_tree};
    use tempfile::TempDir;

    use super::*;

    fn back_date_dir(path: &Path, age: Duration) {
        // Directory mtime: use std::fs::set_modified is unstable, so
        // adjust through utimensat via filetime if needed. Tests use
        // file backdating instead — directory entry's own mtime mirrors
        // its newest contained file once we touch them. To be precise,
        // backdate the entries we created inside the dir.
        let mtime = SystemTime::now() - age;
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            let p = entry.path();
            if p.is_file() {
                let f = std::fs::File::open(&p).unwrap();
                let _ = f.set_modified(mtime);
            }
        }
        // Also touch the directory itself.
        let f = std::fs::File::open(path).unwrap();
        let _ = f.set_modified(mtime);
    }

    fn workspace_paths(root: &Path) -> WorkspacePaths {
        WorkspacePaths::new(root.to_path_buf())
    }

    #[tokio::test]
    async fn log_sweep_removes_old_log_files_in_both_dirs() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let logs_dir = paths.logs_dir();
        let channel_dir = paths.channel_logs_dir();
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::create_dir_all(&channel_dir).unwrap();
        let stale_main = logs_dir.join("baybo.log.2025-01-01");
        let fresh_main = logs_dir.join("baybo.log.2026-04-27");
        let stale_chan = channel_dir.join("telegram.log.2025-01-01");
        std::fs::write(&stale_main, b"x").unwrap();
        std::fs::write(&fresh_main, b"x").unwrap();
        std::fs::write(&stale_chan, b"x").unwrap();
        back_date(
            &stale_main,
            Duration::from_secs(LOG_FILE_TTL.as_secs() + 60),
        );
        back_date(
            &stale_chan,
            Duration::from_secs(LOG_FILE_TTL.as_secs() + 60),
        );

        let report = Janitor::new(paths).sweep_once().await;

        assert_eq!(report.log_files_removed, 2);
        assert!(!stale_main.exists());
        assert!(fresh_main.exists());
        assert!(!stale_chan.exists());
    }

    #[tokio::test]
    async fn work_tmp_sweep_removes_only_wholly_stale_entries() {
        // Layout under work/tmp:
        //   stale.txt   old file                          → removed
        //   stale-dir/  everything inside old             → removed
        //   mixed-dir/  old shell, one fresh file deep in → kept
        //   fresh.txt   new file                          → kept
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let scratch = paths.work_tmp_dir();
        std::fs::create_dir_all(&scratch).unwrap();
        let over_ttl = WORK_TMP_TTL + Duration::from_secs(60);

        let stale_file = scratch.join("stale.txt");
        std::fs::write(&stale_file, b"x").unwrap();
        back_date(&stale_file, over_ttl);

        let stale_dir = scratch.join("stale-dir");
        std::fs::create_dir_all(stale_dir.join("nested")).unwrap();
        std::fs::write(stale_dir.join("nested").join("a.txt"), b"x").unwrap();
        back_date_tree(&stale_dir, over_ttl);

        let mixed_dir = scratch.join("mixed-dir");
        std::fs::create_dir_all(mixed_dir.join("nested")).unwrap();
        std::fs::write(mixed_dir.join("nested").join("old.txt"), b"x").unwrap();
        back_date_tree(&mixed_dir, over_ttl);
        std::fs::write(mixed_dir.join("nested").join("fresh.txt"), b"x").unwrap();

        let fresh_file = scratch.join("fresh.txt");
        std::fs::write(&fresh_file, b"x").unwrap();

        let report = Janitor::new(paths).sweep_once().await;

        assert_eq!(report.work_tmp_removed, 2);
        assert!(!stale_file.exists());
        assert!(!stale_dir.exists());
        assert!(
            mixed_dir.join("nested").join("fresh.txt").exists(),
            "one fresh file must keep the whole entry"
        );
        assert!(fresh_file.exists());
    }

    #[tokio::test]
    async fn work_tmp_sweep_removes_symlinks_as_links_without_traversal() {
        // A stale top-level symlink is unlinked (target untouched); a
        // stale dir containing a symlink to a *fresh* outside target is
        // still removed — proof the walk never follows the link, or the
        // fresh target would have kept the dir alive.
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let scratch = paths.work_tmp_dir();
        std::fs::create_dir_all(&scratch).unwrap();
        let over_ttl = WORK_TMP_TTL + Duration::from_secs(60);

        let target_dir = tmp.path().join("outside");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("fresh.txt"), b"keep").unwrap();

        let top_link = scratch.join("stale-link");
        std::os::unix::fs::symlink(&target_dir, &top_link).unwrap();
        back_date_symlink(&top_link, over_ttl);

        let dir_with_link = scratch.join("stale-dir-with-link");
        std::fs::create_dir_all(&dir_with_link).unwrap();
        let inner_link = dir_with_link.join("link");
        std::os::unix::fs::symlink(&target_dir, &inner_link).unwrap();
        back_date_symlink(&inner_link, over_ttl);
        back_date_tree(&dir_with_link, over_ttl);

        let report = Janitor::new(paths).sweep_once().await;

        assert_eq!(report.work_tmp_removed, 2);
        assert!(
            std::fs::symlink_metadata(&top_link).is_err(),
            "top-level link unlinked"
        );
        assert!(!dir_with_link.exists());
        assert!(
            target_dir.join("fresh.txt").exists(),
            "symlink target must never be deleted"
        );
    }

    #[tokio::test]
    async fn sidecar_cache_sweep_removes_only_stale_non_live_dirs() {
        // Three subdirs under the simulated $XDG_CACHE_HOME/baybo/sidecars/:
        //   browser-livehash : current build, must survive even when stale
        //   browser-oldhash  : prior build, age > TTL, must be removed
        //   browser-recent   : prior build, fresh mtime, must survive
        // Plus a stray non-dir entry (a leftover lockfile) — must survive.
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("sidecars");
        std::fs::create_dir_all(&cache).unwrap();
        let live = cache.join("browser-livehash");
        let stale = cache.join("browser-oldhash");
        let recent = cache.join("browser-recent");
        let stray = cache.join("README");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&recent).unwrap();
        std::fs::write(live.join("bundle.mjs"), b"x").unwrap();
        std::fs::write(stale.join("bundle.mjs"), b"x").unwrap();
        std::fs::write(recent.join("bundle.mjs"), b"x").unwrap();
        std::fs::write(&stray, b"keep").unwrap();
        back_date_dir(&live, Duration::from_secs(SIDECAR_CACHE_TTL.as_secs() + 60));
        back_date_dir(
            &stale,
            Duration::from_secs(SIDECAR_CACHE_TTL.as_secs() + 60),
        );

        let paths = workspace_paths(tmp.path());
        let live_dirs: HashSet<String> = ["browser-livehash".to_string()].into_iter().collect();
        let janitor = Janitor::new(paths).with_sidecar_cache(SidecarCache {
            cache_root: cache.clone(),
            live_dirs,
        });

        let removed = janitor.sweep_sidecar_cache().await;
        assert_eq!(removed, 1, "only the stale non-live dir is removed");
        assert!(
            live.exists(),
            "live dir survives even when its mtime is old"
        );
        assert!(!stale.exists(), "stale non-live dir removed");
        assert!(recent.exists(), "fresh non-live dir survives the TTL guard");
        assert!(stray.exists(), "non-directory entries are left alone");
    }

    #[tokio::test]
    async fn sidecar_cache_sweep_is_noop_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let n = Janitor::new(paths).sweep_sidecar_cache().await;
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn sidecar_cache_sweep_short_circuits_on_missing_root() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("does-not-exist");
        let paths = workspace_paths(tmp.path());
        let janitor = Janitor::new(paths).with_sidecar_cache(SidecarCache {
            cache_root: cache,
            live_dirs: HashSet::new(),
        });
        assert_eq!(janitor.sweep_sidecar_cache().await, 0);
    }

    #[tokio::test]
    async fn sweep_short_circuits_when_target_dirs_do_not_exist() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let report = Janitor::new(paths).sweep_once().await;
        assert_eq!(report.log_files_removed, 0);
    }

    #[tokio::test]
    async fn the_build_artifact_sweep_runs_only_when_it_is_wired() {
        struct Recording(Arc<parking_lot::Mutex<Vec<Duration>>>);

        #[async_trait::async_trait]
        impl BuildArtifactSource for Recording {
            async fn reclaim_idle(&self, idle_for: Duration) -> ReclaimedArtifacts {
                self.0.lock().push(idle_for);
                ReclaimedArtifacts {
                    dirs_removed: 2,
                    bytes_freed: 5_000_000_000,
                }
            }
        }

        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());

        let unwired = Janitor::new(paths.clone()).sweep_once().await;
        assert_eq!(
            (unwired.build_dirs_removed, unwired.build_bytes_freed),
            (0, 0),
            "a workspace with no boards sweeps no checkouts"
        );

        let asked: Arc<parking_lot::Mutex<Vec<Duration>>> = Arc::default();
        let report = Janitor::new(paths)
            .with_build_artifacts(Arc::new(Recording(Arc::clone(&asked))))
            .sweep_once()
            .await;
        assert_eq!(
            *asked.lock(),
            vec![IDLE_CHECKOUT_TTL],
            "asked once, with the TTL"
        );
        assert_eq!(report.build_dirs_removed, 2);
        assert_eq!(report.build_bytes_freed, 5_000_000_000);
    }

    #[tokio::test]
    async fn run_loop_exits_promptly_on_shutdown() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let janitor = Janitor::new(paths);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            janitor
                .run(async move {
                    let _ = rx.await;
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(());
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(outcome.is_ok(), "janitor.run did not honour shutdown");
    }
}
