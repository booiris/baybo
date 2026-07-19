use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::error::JanitorError;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirSweep<'a, F: Fn(&str) -> bool> {
    pub dir: &'a Path,
    pub ttl: Duration,
    pub name_predicate: F,
}

/// Walk `plan.dir`, removing **regular files** whose name matches
/// `plan.name_predicate` and whose mtime is older than `plan.ttl`. The
/// janitor doesn't sweep whole subtrees today — uv manages its own
/// cache eviction (`uv cache prune`), and session-log + log-file
/// sweeps are file-shaped.
pub(crate) async fn sweep_directory<F>(plan: DirSweep<'_, F>) -> Result<usize, JanitorError>
where
    F: Fn(&str) -> bool,
{
    let mut reader = match tokio::fs::read_dir(plan.dir).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(JanitorError::fs(plan.dir.display(), e)),
    };

    let now = SystemTime::now();
    let mut removed = 0usize;
    loop {
        let entry = match reader.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return Err(JanitorError::fs(plan.dir.display(), e)),
        };

        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !(plan.name_predicate)(name_str) {
            continue;
        }

        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(JanitorError::fs(entry.path().display(), e)),
        };

        if !metadata.is_file() {
            continue;
        }

        let modified = match metadata.modified() {
            Ok(t) => t,
            Err(e) => return Err(JanitorError::fs(entry.path().display(), e)),
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < plan.ttl {
            continue;
        }

        let path = entry.path();
        match tokio::fs::remove_file(&path).await {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "janitor failed to remove entry",
                );
            }
        }
    }
    Ok(removed)
}

/// Remove every top-level entry of `dir` whose newest mtime anywhere in
/// its tree is older than `ttl`. A directory entry is removed whole
/// (`remove_dir_all`); files and symlinks go through `remove_file`, so a
/// symlinked entry is removed as a link and its target is never touched.
/// Missing `dir` is a no-op; per-entry failures are logged and skipped.
pub(crate) async fn sweep_tree_entries(dir: &Path, ttl: Duration) -> Result<usize, JanitorError> {
    let mut reader = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(JanitorError::fs(dir.display(), e)),
    };

    let now = SystemTime::now();
    let mut removed = 0usize;
    loop {
        let entry = match reader.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return Err(JanitorError::fs(dir.display(), e)),
        };
        let path = entry.path();
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "janitor failed to stat scratch entry",
                );
                continue;
            }
        };
        let newest = match newest_mtime(&path, &metadata).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "janitor failed to scan scratch entry",
                );
                continue;
            }
        };
        let age = now.duration_since(newest).unwrap_or(Duration::ZERO);
        if age < ttl {
            continue;
        }

        let result = if metadata.file_type().is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "janitor failed to remove entry",
                );
            }
        }
    }
    Ok(removed)
}

/// Newest mtime observed under `path` without following symlinks: the
/// entry's own lstat mtime plus — for a real directory — the lstat
/// mtime of everything beneath it (directories included, so a create or
/// delete inside refreshes its parent). A symlink contributes only the
/// link's own mtime.
async fn newest_mtime(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<SystemTime, JanitorError> {
    let mut newest = metadata
        .modified()
        .map_err(|e| JanitorError::fs(path.display(), e))?;
    if !metadata.file_type().is_dir() {
        return Ok(newest);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut reader = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| JanitorError::fs(dir.display(), e))?;
        loop {
            let entry = match reader.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => return Err(JanitorError::fs(dir.display(), e)),
            };
            let child = entry.path();
            let meta = match tokio::fs::symlink_metadata(&child).await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(JanitorError::fs(child.display(), e)),
            };
            if let Ok(t) = meta.modified()
                && t > newest
            {
                newest = t;
            }
            if meta.file_type().is_dir() {
                stack.push(child);
            }
        }
    }
    Ok(newest)
}

pub(crate) fn is_log_file(name: &str) -> bool {
    // Daily-rolling tracing files plus channel sidecar variants.
    // Examples: `baybo.log.2026-04-27`, `telegram.log.2026-04-27`.
    let bytes = name.as_bytes();
    if bytes.len() < 5 {
        return false;
    }
    if name.contains(".log.") {
        return true;
    }
    name.ends_with(".log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_predicates_match_expected_shapes() {
        assert!(is_log_file("baybo.log.2026-04-27"));
        assert!(is_log_file("telegram.log.2026-04-27"));
        assert!(is_log_file("plain.log"));
        assert!(!is_log_file("README.md"));
    }
}
