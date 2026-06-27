//! Read-before-write tracking for the file-mutating tools.
//!
//! Enforces a two-part contract on `Edit` (always) and `Write` (when it
//! would overwrite an existing file): the agent must have `Read` the file
//! in this session **and** the file must not have changed on disk since
//! that read. It stops two real failure modes — editing a file the model
//! never looked at (clobbering content it can't see), and editing against
//! a stale view after the user, a formatter, or another tool rewrote the
//! file mid-conversation.
//!
//! The [`ReadTracker`] is a shared, cheap-to-clone handle (`Arc<Mutex<…>>`)
//! threaded through [`crate::ToolContext`]. It is created per session and
//! lives on the long-lived agent loop, so a `Read` in one turn satisfies an
//! `Edit` in a later turn. Across an actor eviction or process restart it is
//! rebuilt from the persisted transcript by [`ReadTracker::rebuild_from_messages`]:
//! each `Read` result row carries the [`FileFingerprint`] it observed in its
//! [`ToolResultMeta`] (persisted with the transcript but never sent to the LLM),
//! so the prior reads are recovered on hydration. If a fingerprint is ever lost
//! (a `Read` row compacted away, a stale rebuild) the contract just fails
//! closed — the model is forced to re-read, never allowed a blind write.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::{ChatMessage, ContentBlock, FileFingerprint};
use parking_lot::Mutex;

/// Outcome of checking a file against the tracker before a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadCheck {
    /// A recorded read still matches the file on disk — the write may proceed.
    Current,
    /// No read of this file is recorded in the session.
    NeverRead,
    /// A read is recorded but the file changed since (fingerprint differs).
    Stale,
}

impl ReadCheck {
    /// Remediation message for a failed check, or `None` when the read is
    /// current (the write may proceed). `action` completes the sentence
    /// "Read it before {action}" — e.g. `"editing it"`,
    /// `"overwriting it with Write"`.
    pub(crate) fn rejection(self, path: &Path, action: &str) -> Option<String> {
        let path = path.display();
        match self {
            ReadCheck::Current => None,
            ReadCheck::NeverRead => Some(format!(
                "{path} has not been read in this session — use the Read tool on it before \
                 {action}. Editing a file you have not read risks clobbering content you \
                 cannot see."
            )),
            ReadCheck::Stale => Some(format!(
                "{path} has changed on disk since you last read it (edited by the user, a \
                 formatter, or another tool) — use the Read tool on it again before {action} \
                 so your change applies to the current content."
            )),
        }
    }
}

/// Session-scoped record of which files have been read and their
/// fingerprint at read time. Cheap to clone (one `Arc` bump); all clones
/// share the same map.
///
/// Keyed by the canonicalised path so a `Read` and a later `Edit` that
/// spell the path differently (a `..` segment, a symlink) still agree on
/// the same entry.
#[derive(Clone, Default)]
pub struct ReadTracker {
    inner: Arc<Mutex<HashMap<PathBuf, FileFingerprint>>>,
}

impl ReadTracker {
    /// Canonical map key for `path`. Falls back to the path as-given when
    /// canonicalisation fails (file removed mid-operation) so the lookup is
    /// still deterministic — a `Read` and `Edit` racing a delete just miss
    /// each other and the contract fails closed (`NeverRead`).
    fn key(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Record that `path` was read, with the fingerprint observed at read
    /// time. Overwrites any prior entry (a re-read re-anchors the baseline).
    pub(crate) fn record(&self, path: &Path, fingerprint: FileFingerprint) {
        let key = Self::key(path);
        self.inner.lock().insert(key, fingerprint);
    }

    /// `stat` `path` and record its current fingerprint as read. Best-effort:
    /// a stat failure leaves the tracker unchanged. Used by `Read` after a
    /// successful read and by `Edit`/`Write` to re-anchor their own write so
    /// a chained edit does not demand an intervening re-read.
    pub(crate) fn record_from_disk(&self, path: &Path) {
        if let Ok(meta) = std::fs::metadata(path) {
            self.record(path, FileFingerprint::from_metadata(&meta));
        }
    }

    /// Compare a recorded read against the file's `current` fingerprint.
    /// The caller supplies `current` (it has just `stat`'d the file it is
    /// about to write), keeping this method free of I/O and trivially
    /// testable.
    pub(crate) fn check(&self, path: &Path, current: FileFingerprint) -> ReadCheck {
        let key = Self::key(path);
        match self.inner.lock().get(&key) {
            None => ReadCheck::NeverRead,
            Some(recorded) if *recorded == current => ReadCheck::Current,
            Some(_) => ReadCheck::Stale,
        }
    }

    /// The fingerprint recorded for `path`, if any. The agent loop reads this
    /// right after a `Read` to stamp the value onto the persisted
    /// `ToolResult` row (so it can be rebuilt on hydration). `pub` because the
    /// caller is in `baybo-agent`.
    pub fn get(&self, path: &Path) -> Option<FileFingerprint> {
        let key = Self::key(path);
        self.inner.lock().get(&key).copied()
    }

    /// Repopulate the tracker from a restored transcript: pair each `Read`
    /// `ToolUse` (for its `file_path`) with its `ToolResult` (for the fingerprint
    /// its [`ToolResultMeta`](baybo_model::ToolResultMeta) carried) and record
    /// it. Later reads of the same file overwrite earlier ones, since messages
    /// are walked in order.
    /// Called once after the agent loop restores its context from the store, so
    /// a `Read` that happened before an eviction/restart still satisfies a
    /// later `Edit`. `pub` because the caller is in `baybo-agent`.
    pub fn rebuild_from_messages(&self, messages: &[ChatMessage]) {
        let mut read_paths: HashMap<&str, &str> = HashMap::new();
        for msg in messages {
            for block in &msg.content {
                if let ContentBlock::ToolUse {
                    id, name, input, ..
                } = block
                    && name == crate::READ_TOOL_NAME
                    && let Some(path) = input.get("file_path").and_then(|v| v.as_str())
                {
                    read_paths.insert(id.as_str(), path);
                }
            }
        }
        for msg in messages {
            for block in &msg.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    meta: Some(meta),
                    ..
                } = block
                    && let Some(fp) = meta.read_fingerprint
                    && let Some(path) = read_paths.get(tool_use_id.as_str())
                {
                    self.record(Path::new(path), fp);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    fn fp(mtime_secs: u64, size: u64) -> FileFingerprint {
        FileFingerprint::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
            size,
        )
    }

    #[test]
    fn unread_file_reports_never_read() {
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "x").unwrap();
        assert_eq!(t.check(&p, fp(1, 1)), ReadCheck::NeverRead);
    }

    #[test]
    fn recorded_then_unchanged_is_current() {
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello").unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        let current = FileFingerprint::from_metadata(&meta);
        t.record(&p, current);
        assert_eq!(t.check(&p, current), ReadCheck::Current);
    }

    #[test]
    fn size_change_is_detected_as_stale() {
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello").unwrap();
        t.record_from_disk(&p);
        // A different length flips the fingerprint regardless of mtime
        // resolution, so the check is deterministic.
        std::fs::write(&p, "hello world, much longer").unwrap();
        let now = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());
        assert_eq!(t.check(&p, now), ReadCheck::Stale);
    }

    #[test]
    fn record_from_disk_refresh_makes_subsequent_check_current() {
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "one").unwrap();
        t.record_from_disk(&p);
        // Simulate a tool rewriting the file and re-anchoring afterwards.
        std::fs::write(&p, "two longer").unwrap();
        t.record_from_disk(&p);
        let now = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());
        assert_eq!(t.check(&p, now), ReadCheck::Current);
    }

    #[test]
    fn key_normalises_dotdot_segments() {
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let p = sub.join("a.txt");
        std::fs::write(&p, "hi").unwrap();
        let current = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());
        t.record(&p, current);
        // Same file reached via a `..` round-trip resolves to the same key.
        let spelled = sub.join("..").join("sub").join("a.txt");
        assert_eq!(t.check(&spelled, current), ReadCheck::Current);
    }

    #[test]
    fn clones_share_one_map() {
        let a = ReadTracker::default();
        let b = a.clone();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "x").unwrap();
        let current = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());
        a.record(&p, current);
        assert_eq!(b.check(&p, current), ReadCheck::Current);
    }

    #[test]
    fn rebuild_from_messages_recovers_reads() {
        // A transcript with a Read (ToolUse + ToolResult carrying a
        // fingerprint) rebuilds the tracker so a later check passes without a
        // fresh Read — mimicking hydration after a restart.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello").unwrap();
        let recorded = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());

        let messages = vec![
            ChatMessage::assistant(vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: crate::READ_TOOL_NAME.into(),
                input: json!({ "file_path": p.to_str().unwrap() }),
                signature: None,
            }]),
            ChatMessage::tool_result_with_meta(
                "call-1".into(),
                "…file body…".into(),
                Some(baybo_model::ToolResultMeta {
                    read_fingerprint: Some(recorded),
                }),
            ),
        ];

        let t = ReadTracker::default();
        t.rebuild_from_messages(&messages);
        assert_eq!(t.check(&p, recorded), ReadCheck::Current);
    }

    #[test]
    fn rebuild_ignores_results_without_fingerprint() {
        // A non-Read tool result (no fingerprint) must not seed the tracker.
        let messages = vec![ChatMessage::tool_result("call-x".into(), "ok".into())];
        let t = ReadTracker::default();
        t.rebuild_from_messages(&messages);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "x").unwrap();
        assert_eq!(t.check(&p, fp(1, 1)), ReadCheck::NeverRead);
    }

    #[test]
    fn rejection_messages_match_state() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        assert!(ReadCheck::Current.rejection(&p, "editing it").is_none());
        assert!(
            ReadCheck::NeverRead
                .rejection(&p, "editing it")
                .unwrap()
                .contains("has not been read")
        );
        assert!(
            ReadCheck::Stale
                .rejection(&p, "editing it")
                .unwrap()
                .contains("changed on disk")
        );
    }
}
