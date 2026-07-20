mod error;
mod fs_sweep;
mod sidecar_sweep;

pub use error::JanitorError;

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baybo_store::{BlobStore, ChannelPairingStore, DeckCardStore};
use baybo_workspace::WorkspacePaths;

use fs_sweep::{DirSweep, is_log_file, sweep_directory};

const LOG_FILE_TTL: Duration = Duration::from_secs(30 * 86_400);
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
const TICK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
// Deck-blob TTL. A service-produced `deck:*` blob unreferenced by any retained
// snapshot for this long is dead garbage (a refresh-loop card rotates old
// snapshot refs out of its keep-window). `last_accessed_at` is deliberately NOT
// consulted — the device cache is content-addressed and never re-fetches, so a
// blob a card renders daily still shows no server access. Long enough that a
// transient display blob (delivered in a call result, never snapshotted) that a
// card meant to keep has ample time to appear in a snapshot first.
const DECK_BLOB_TTL: Duration = Duration::from_secs(7 * 86_400);
// Sidecar-cache cleanup is much rarer than the other sweeps — it only
// reclaims after a binary upgrade lands a fresh content hash, and the
// total cruft per upgrade is single-digit MB. Daily cadence keeps the
// walk off the hot path while still bounding the eventual size.
const SIDECAR_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default, Clone, Copy)]
pub struct JanitorReport {
    pub log_files_removed: usize,
    pub sidecar_dirs_removed: usize,
    pub pairings_purged: u64,
    pub deck_blobs_purged: u64,
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
    deck_blob: Option<Arc<dyn BlobStore>>,
    deck_store: Option<Arc<dyn DeckCardStore>>,
}

impl Janitor {
    pub fn new(paths: WorkspacePaths) -> Self {
        Self {
            paths,
            sidecar_cache: None,
            pairings: None,
            deck_blob: None,
            deck_store: None,
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

    /// Wire the blob + deck stores for the deck-blob sweep (docs/modules/deck.md
    /// §Blobs). Without this call the deck-blob sweep doesn't run. Only `deck:*`
    /// blobs are ever touched — chat blobs and picker `deck-user:*` uploads are
    /// never in range.
    #[must_use]
    pub fn with_deck_blobs(
        mut self,
        blob: Arc<dyn BlobStore>,
        deck: Arc<dyn DeckCardStore>,
    ) -> Self {
        self.deck_blob = Some(blob);
        self.deck_store = Some(deck);
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

        // Pairings get a daily sweep here too so a long-running process
        // that never trips the hourly tick (e.g. heavy load deferring
        // every interval fire) still eventually reaps stale rows.
        if self.pairings.is_some() {
            report.pairings_purged += self.sweep_pairings_once(chrono::Utc::now()).await;
        }

        report.deck_blobs_purged += self
            .sweep_deck_blobs_once(chrono::Utc::now().timestamp_micros())
            .await;

        tracing::info!(
            log_files_removed = report.log_files_removed,
            pairings_purged = report.pairings_purged,
            deck_blobs_purged = report.deck_blobs_purged,
            "janitor sweep complete",
        );

        report
    }

    /// Delete `deck:*` blobs older than [`DECK_BLOB_TTL`] that no retained
    /// snapshot (binned cards included) still references. `now_us` is unix µs.
    /// Best-effort — a failed step logs and the sweep moves on; deck blob GC is
    /// never a correctness dependency. No-op unless [`Self::with_deck_blobs`]
    /// wired the stores.
    pub async fn sweep_deck_blobs_once(&self, now_us: i64) -> u64 {
        let (Some(blob), Some(deck)) = (&self.deck_blob, &self.deck_store) else {
            return 0;
        };
        let cutoff = now_us - DECK_BLOB_TTL.as_micros() as i64;
        let candidates = match blob.list_ids_by_uploader("deck:", Some(cutoff)).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "deck-blob sweep: list failed");
                return 0;
            }
        };
        let mut purged = 0u64;
        for id in candidates {
            match deck.snapshot_references(&id).await {
                Ok(true) => continue, // still live in some (maybe binned) snapshot
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "deck-blob sweep: reference check failed");
                    continue;
                }
            }
            match blob.delete(&id).await {
                Ok(()) => purged += 1,
                Err(e) => tracing::warn!(error = %e, "deck-blob sweep: delete failed"),
            }
        }
        purged
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
        tracing::info!(tick_secs = TICK_INTERVAL.as_secs(), "janitor started",);
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
                    tracing::info!("janitor shutting down");
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

    use tempfile::TempDir;

    use super::*;

    fn back_date(path: &Path, age: Duration) {
        let mtime = SystemTime::now() - age;
        let file = std::fs::File::open(path).unwrap();
        file.set_modified(mtime).unwrap();
    }

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
    async fn deck_blob_sweep_respects_ttl_reference_and_prefix() {
        use baybo_storage::sqlite::{SqliteBlobStore, SqliteDeckCardStore, SqlitePool};
        use baybo_store::{DeckCardRow, DeckSize};

        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let blob = Arc::new(
            SqliteBlobStore::open(pool.clone(), tmp.path().join("blobs"))
                .await
                .unwrap(),
        );
        let deck = Arc::new(SqliteDeckCardStore::new(pool));
        deck.create(&DeckCardRow {
            id: "card1".into(),
            title: "card1".into(),
            position: 0,
            size: DeckSize::Wide,
            sizes: vec![DeckSize::Wide],
            maximize: false,
            enabled: true,
            quarantined_at: None,
            deleted_at: None,
            spec_hash: "h".into(),
            last_seq: 0,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

        let unref = blob
            .put(b"unref", "text/plain", Some("deck:card1"))
            .await
            .unwrap();
        let refd = blob
            .put(b"refd", "text/plain", Some("deck:card1"))
            .await
            .unwrap();
        let dev = blob
            .put(b"chat", "text/plain", Some("device:x"))
            .await
            .unwrap();
        // A snapshot points at `refd` — the sweep must keep it.
        deck.record_snapshot(
            "card1",
            &format!("{{\"img\":\"{}\"}}", refd.blob_id),
            None,
            chrono::Utc::now(),
        )
        .await
        .unwrap();

        let j = Janitor::new(paths).with_deck_blobs(blob.clone(), deck.clone());
        // A real-now sweep finds nothing older than the 7-day TTL.
        assert_eq!(
            j.sweep_deck_blobs_once(chrono::Utc::now().timestamp_micros())
                .await,
            0
        );
        // 8 days on, every blob qualifies by age — but only the unreferenced
        // deck: one is swept; the referenced deck: blob and the chat blob stay.
        let future = chrono::Utc::now().timestamp_micros() + 8 * 86_400 * 1_000_000;
        assert_eq!(j.sweep_deck_blobs_once(future).await, 1);
        assert!(blob.get(&unref.blob_id).await.is_err());
        assert!(blob.get(&refd.blob_id).await.is_ok());
        assert!(blob.get(&dev.blob_id).await.is_ok());
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
