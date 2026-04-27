mod blob_sweep;
mod error;
mod fs_sweep;

pub use error::JanitorError;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aura_storage::BlobStore;
use aura_workspace::WorkspacePaths;

use fs_sweep::{DirSweep, EntryShape, is_log_file, is_session_log, is_uuid_dir, sweep_directory};

const SESSION_LOG_TTL: Duration = Duration::from_secs(3 * 86_400);
const PYTHON_SCRATCH_TTL: Duration = Duration::from_secs(3 * 86_400);
const LOG_FILE_TTL: Duration = Duration::from_secs(14 * 86_400);
// LRU window for blobs — touched on every successful stat/get/open. A
// blob whose `last_accessed_at` falls behind this window is reaped on
// the next sweep.
const BLOB_TTL: Duration = Duration::from_secs(7 * 86_400);
const TICK_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Default, Clone, Copy)]
pub struct JanitorReport {
    pub session_logs_removed: usize,
    pub python_dirs_removed: usize,
    pub log_files_removed: usize,
    pub blobs_purged: u64,
}

pub struct Janitor {
    paths: WorkspacePaths,
    blobs: Arc<dyn BlobStore>,
}

impl Janitor {
    pub fn new(paths: WorkspacePaths, blobs: Arc<dyn BlobStore>) -> Self {
        Self { paths, blobs }
    }

    /// Run all four sweeps once. Failures in one sweep are logged and
    /// the others still run — janitor is best-effort.
    pub async fn sweep_once(&self) -> JanitorReport {
        let mut report = JanitorReport::default();

        let sessions_dir = self.paths.sessions_log_dir();
        match sweep_directory(DirSweep {
            dir: &sessions_dir,
            ttl: SESSION_LOG_TTL,
            shape: EntryShape::File,
            name_predicate: is_session_log,
        })
        .await
        {
            Ok(n) => report.session_logs_removed = n,
            Err(e) => tracing::warn!(error = %e, "session-log sweep failed"),
        }

        let scratch_dir = self.paths.code_builder_dir();
        match sweep_directory(DirSweep {
            dir: &scratch_dir,
            ttl: PYTHON_SCRATCH_TTL,
            shape: EntryShape::Dir,
            name_predicate: is_uuid_dir,
        })
        .await
        {
            Ok(n) => report.python_dirs_removed = n,
            Err(e) => tracing::warn!(error = %e, "python-scratch sweep failed"),
        }

        let mut logs_total = 0;
        for dir in [self.paths.logs_dir(), self.paths.channel_logs_dir()] {
            match sweep_directory(DirSweep {
                dir: &dir,
                ttl: LOG_FILE_TTL,
                shape: EntryShape::File,
                name_predicate: is_log_file,
            })
            .await
            {
                Ok(n) => logs_total += n,
                Err(e) => tracing::warn!(error = %e, dir = %dir.display(), "log sweep failed"),
            }
        }
        report.log_files_removed = logs_total;

        match blob_sweep::purge_old_blobs(&self.blobs, BLOB_TTL).await {
            Ok(n) => report.blobs_purged = n,
            Err(e) => tracing::warn!(error = %e, "blob sweep failed"),
        }

        tracing::info!(
            session_logs_removed = report.session_logs_removed,
            python_dirs_removed = report.python_dirs_removed,
            log_files_removed = report.log_files_removed,
            blobs_purged = report.blobs_purged,
            "janitor sweep complete",
        );

        report
    }

    /// Background loop: sweep at boot, then every [`TICK_INTERVAL`].
    /// Returns when `shutdown` resolves. Designed to be wrapped in a
    /// `tokio::spawn` next to the cron tick loop.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        // First tick fires immediately; subsequent ticks delay rather
        // than burst-catch-up so a slow sweep can't stack.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::info!(tick_secs = TICK_INTERVAL.as_secs(), "janitor started",);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let _ = self.sweep_once().await;
                }
                _ = &mut shutdown => {
                    tracing::info!("janitor shutting down");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::SystemTime;

    use aura_storage::BlobStore;
    use aura_storage::test_support::MemoryBlobStore;
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
    async fn session_log_sweep_removes_old_jsonl_files_only() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let dir = paths.sessions_log_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("old-session.jsonl");
        let fresh = dir.join("new-session.jsonl");
        let other = dir.join("notes.txt");
        std::fs::write(&stale, b"{}").unwrap();
        std::fs::write(&fresh, b"{}").unwrap();
        std::fs::write(&other, b"keep").unwrap();
        back_date(&stale, Duration::from_secs(SESSION_LOG_TTL.as_secs() + 60));

        let report = Janitor::new(paths, Arc::new(MemoryBlobStore::new()))
            .sweep_once()
            .await;

        assert_eq!(report.session_logs_removed, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(other.exists(), "non-jsonl files must survive");
    }

    #[tokio::test]
    async fn python_scratch_sweep_removes_old_uuid_dirs_only() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let dir = paths.code_builder_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("00000000-0000-0000-0000-000000000001");
        let fresh = dir.join("00000000-0000-0000-0000-000000000002");
        let other = dir.join("not-a-uuid");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(stale.join("script.py"), b"print(1)\n").unwrap();
        std::fs::write(fresh.join("script.py"), b"print(2)\n").unwrap();
        // Make stale dir mtime old by setting on its inner file then
        // re-stating: tokio::fs uses the dir entry's mtime, which on
        // Linux is set when the dir is last modified (e.g., when we
        // wrote script.py). Backdate via set_modified on the dir.
        back_date_dir(
            &stale,
            Duration::from_secs(PYTHON_SCRATCH_TTL.as_secs() + 60),
        );

        let report = Janitor::new(paths, Arc::new(MemoryBlobStore::new()))
            .sweep_once()
            .await;

        assert_eq!(report.python_dirs_removed, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(other.exists(), "non-UUID dirs must survive");
    }

    #[tokio::test]
    async fn log_sweep_removes_old_log_files_in_both_dirs() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let logs_dir = paths.logs_dir();
        let channel_dir = paths.channel_logs_dir();
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::create_dir_all(&channel_dir).unwrap();
        let stale_main = logs_dir.join("aura.log.2025-01-01");
        let fresh_main = logs_dir.join("aura.log.2026-04-27");
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

        let report = Janitor::new(paths, Arc::new(MemoryBlobStore::new()))
            .sweep_once()
            .await;

        assert_eq!(report.log_files_removed, 2);
        assert!(!stale_main.exists());
        assert!(fresh_main.exists());
        assert!(!stale_chan.exists());
    }

    #[tokio::test]
    async fn blob_sweep_purges_old_blob_rows_via_store_trait() {
        // Reaches the BlobStore trait through `Arc<dyn BlobStore>` —
        // same dispatch the gateway uses. The libsql path's
        // on-disk-unlink behaviour is covered separately in
        // `crates/storage/src/libsql/blob.rs` tests.
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());

        let memory = Arc::new(MemoryBlobStore::new());
        let store: Arc<dyn BlobStore> = Arc::clone(&memory) as _;
        let _ = store.put(b"fresh", "text/plain", None).await.unwrap();

        // The MemoryBlobStore stamps `created_at = now` on every put and
        // exposes no clock seam; pass a cutoff well into the future so
        // the live row is unconditionally older than it.
        let future_cutoff = chrono::Utc::now().timestamp() + 86_400;
        let report = Janitor::new(paths.clone(), Arc::clone(&store))
            .sweep_once_with_blob_cutoff(future_cutoff)
            .await;
        assert_eq!(report.blobs_purged, 1);
        assert_eq!(memory.len(), 0, "row should be soft-deleted");

        // A second run is idempotent.
        let report2 = Janitor::new(paths, Arc::clone(&store)).sweep_once().await;
        assert_eq!(report2.blobs_purged, 0);
    }

    #[tokio::test]
    async fn sweep_short_circuits_when_target_dirs_do_not_exist() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let report = Janitor::new(paths, Arc::new(MemoryBlobStore::new()))
            .sweep_once()
            .await;
        assert_eq!(report.session_logs_removed, 0);
        assert_eq!(report.python_dirs_removed, 0);
        assert_eq!(report.log_files_removed, 0);
        assert_eq!(report.blobs_purged, 0);
    }

    #[tokio::test]
    async fn run_loop_exits_promptly_on_shutdown() {
        let tmp = TempDir::new().unwrap();
        let paths = workspace_paths(tmp.path());
        let janitor = Janitor::new(paths, Arc::new(MemoryBlobStore::new()));
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

    impl Janitor {
        async fn sweep_once_with_blob_cutoff(&self, cutoff: i64) -> JanitorReport {
            let mut report = JanitorReport::default();
            match self.blobs.purge_older_than(cutoff).await {
                Ok(n) => report.blobs_purged = n,
                Err(e) => tracing::warn!(error = %e, "test blob sweep failed"),
            }
            report
        }
    }
}
