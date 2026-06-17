//! Per-workspace singleton guard.
//!
//! The chat loop owns the workspace's storage, job manager, and cron queue.
//! Running two chat loops against the same workspace would race on those
//! resources (libsql writes, cron tick loops, job recovery), so we serialize
//! with an advisory `flock` on `<workspace>/state/aura.lock`. The lock is
//! held by an open `File` and released when the process exits — even on
//! crash, since the kernel drops the lock with the fd.
//!
//! Scope is deliberately per-workspace: separate workspace dirs should be
//! free to run their own aura concurrently.

use std::fs::{OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;

use aura_workspace::WorkspacePaths;

/// RAII guard that holds the workspace lock for its lifetime.
pub struct WorkspaceLock {
    // The file must stay open to hold the flock; `_file` communicates intent.
    _file: std::fs::File,
}

/// Try to acquire the workspace singleton lock. Returns an error if another
/// aura process already holds it, or if the lock file cannot be opened.
pub fn acquire(workspace_root: &Path) -> anyhow::Result<WorkspaceLock> {
    let path = WorkspacePaths::new(workspace_root.to_path_buf()).singleton_lock();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("failed to open lock file {}: {e}", path.display()))?;

    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let holder = std::fs::read_to_string(&path).unwrap_or_default();
            let holder = holder.trim();
            let detail = if holder.is_empty() {
                String::new()
            } else {
                format!(" (held by pid {holder})")
            };
            anyhow::bail!(
                "another aura instance is running for workspace {}{detail}; \
                 lock file: {}",
                workspace_root.display(),
                path.display(),
            );
        }
        Err(TryLockError::Error(e)) => {
            anyhow::bail!("failed to lock {}: {e}", path.display());
        }
    }

    // Best-effort: record our pid for diagnostics. Failure here does not
    // invalidate the lock (the flock is already held).
    let pid = std::process::id();
    let _ = file.set_len(0);
    let _ = writeln!(file, "{pid}");

    Ok(WorkspaceLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn acquire_then_second_attempt_fails() {
        let dir = tempdir();
        let first = acquire(&dir).expect("first acquire");
        let err = match acquire(&dir) {
            Ok(_) => panic!("second acquire must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("another aura instance is running"),
            "unexpected error: {err}"
        );
        drop(first);
        // Once dropped, acquisition succeeds again.
        acquire(&dir).expect("re-acquire after drop");
    }

    #[test]
    fn lock_file_contains_pid() {
        let dir = tempdir();
        let _lock = acquire(&dir).expect("acquire");
        let lock_path = WorkspacePaths::new(dir.clone()).singleton_lock();
        let contents = std::fs::read_to_string(&lock_path).expect("read lock file");
        let pid: u32 = contents.trim().parse().expect("pid should parse");
        assert_eq!(pid, std::process::id());
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "aura-singleton-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
