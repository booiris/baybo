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
