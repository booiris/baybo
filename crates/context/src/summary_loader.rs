//! Filesystem reader for the per-session `summary.md` file written by
//! the background refresh runner.

use std::path::PathBuf;

use aura_model::SessionId;

// Mirrors `aura_workspace::SUMMARY_FILE`; kept local so this crate
// stays free of an `aura-workspace` dep.
const SUMMARY_FILE_NAME: &str = "summary.md";

pub(crate) struct FsSummaryLoader {
    base_dir: PathBuf,
}

impl FsSummaryLoader {
    pub(crate) fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// `Ok(None)` when the file does not exist (fresh session, never
    /// crossed the trigger threshold). `Err` only for genuine I/O
    /// faults; the caller logs and falls through.
    pub(crate) async fn load(&self, session_id: &SessionId) -> std::io::Result<Option<String>> {
        let path = self
            .base_dir
            .join(session_id.as_str())
            .join(SUMMARY_FILE_NAME);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fs_loader_round_trips_and_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionId::from("abc");
        let session_dir = dir.path().join(session_id.as_str());
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.md"), "hello world").unwrap();

        let loader = FsSummaryLoader::new(dir.path().to_path_buf());
        assert_eq!(
            loader.load(&session_id).await.unwrap().as_deref(),
            Some("hello world")
        );

        let missing = SessionId::from("does-not-exist");
        assert_eq!(loader.load(&missing).await.unwrap(), None);
    }
}
