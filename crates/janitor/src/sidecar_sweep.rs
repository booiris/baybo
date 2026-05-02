use std::time::{Duration, SystemTime};

use crate::SidecarCache;
use crate::error::JanitorError;

pub(crate) async fn sweep(cache: &SidecarCache, ttl: Duration) -> Result<usize, JanitorError> {
    let mut reader = match tokio::fs::read_dir(&cache.cache_root).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(JanitorError::fs(cache.cache_root.display(), e)),
    };

    let now = SystemTime::now();
    let mut removed = 0usize;
    loop {
        let entry = match reader.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return Err(JanitorError::fs(cache.cache_root.display(), e)),
        };

        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if cache.live_dirs.contains(name_str) {
            continue;
        }

        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(JanitorError::fs(entry.path().display(), e)),
        };
        if !metadata.is_dir() {
            continue;
        }

        let modified = match metadata.modified() {
            Ok(t) => t,
            Err(e) => return Err(JanitorError::fs(entry.path().display(), e)),
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < ttl {
            continue;
        }

        let path = entry.path();
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "janitor failed to remove stale sidecar dir",
                );
            }
        }
    }
    Ok(removed)
}
