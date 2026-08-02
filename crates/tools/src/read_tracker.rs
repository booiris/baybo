//! Read-before-write tracking for the file-mutating tools.
//!
//! Enforces a two-part contract on `Edit` (always) and `Write` (when it
//! would overwrite an existing file): the agent must have `Read` the file
//! in this session **and** the file must not have changed on disk since
//! that read. It stops three real failure modes — editing a file the model
//! never looked at (clobbering content it can't see), editing against
//! a stale view after the user, a formatter, or another tool rewrote the
//! file mid-conversation, and "authorizing" a write with a `Read` issued in
//! the *same* LLM response (the model emitted that write before it could see
//! the read's result, so the write is blind — handled by staging reads and
//! promoting them only at the next response boundary, see
//! [`ReadTracker::begin_response`]).
//!
//! The [`ReadTracker`] is a shared, cheap-to-clone handle (`Arc<Mutex<…>>`)
//! threaded through [`crate::ToolContext`]. It is created per session and
//! lives on the long-lived agent loop, so a `Read` in one turn satisfies an
//! `Edit` in a later turn. Across an actor eviction or process restart it is
//! rebuilt from the persisted transcript by [`ReadTracker::rebuild_from_messages`]:
//! every anchoring tool's result row ([`crate::READ_TRACKER_ANCHORING_TOOLS`] —
//! `Read` for what it saw, `Edit`/`Write` for what they wrote) carries the
//! [`FileFingerprint`] it left here in its [`ToolResultMeta`] (persisted with the
//! transcript but never sent to the LLM), so the anchors are recovered on
//! hydration. `Edit`/`Write` matter as much as `Read` there: restoring only the
//! read that preceded an edit puts back the *pre-write* fingerprint, which reads
//! as the file having changed behind the model's back. If a fingerprint is ever
//! lost (a row compacted away, a stale rebuild) the contract just fails closed —
//! the model is forced to re-read, never allowed a blind write.

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
                 formatter, another tool, or — for a shared file — another agent) — use the \
                 Read tool on it again before {action} so your change applies to the current \
                 content."
            )),
        }
    }
}

/// Session-scoped record of which files have been read and their fingerprint
/// at read time. Cheap to clone (one `Arc` bump); all clones share the same
/// state. Keyed by the canonicalised path so a `Read` and a later `Edit` that
/// spell the path differently (a `..` segment, a symlink) still agree.
///
/// A read is staged in `pending` and only promoted to `committed` by
/// [`Self::begin_response`] at the start of the next tool-dispatch batch — so a
/// `Read` and an `Edit`/`Write` of the same file issued in the **same** LLM
/// response cannot authorize each other (the model emitted the write before it
/// could see the read's result; the edit is blind). Write-checks consult only
/// `committed`. A tool's own post-write re-anchor goes straight to `committed`,
/// so batched edits to one file within a single response still work.
#[derive(Clone, Default)]
pub struct ReadTracker {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// Reads visible to write-checks: promoted from `pending` at each response
    /// boundary, plus tools' own post-write re-anchors and hydration.
    committed: HashMap<PathBuf, FileFingerprint>,
    /// Reads staged during the in-flight response, not yet visible to checks.
    pending: HashMap<PathBuf, FileFingerprint>,
}

impl ReadTracker {
    /// Canonical map key for `path`. Falls back to the path as-given when
    /// canonicalisation fails (file removed mid-operation) so the lookup is
    /// still deterministic — a `Read` and `Edit` racing a delete just miss
    /// each other and the contract fails closed (`NeverRead`).
    fn key(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Promote the previous response's staged reads into the committed set so
    /// they authorize writes from this response on. Called once at the start of
    /// each tool-dispatch batch by the agent loop, before any tool in the batch
    /// runs. `pub` because the caller is in `baybo-agent`.
    pub fn begin_response(&self) {
        let mut inner = self.inner.lock();
        let promoted = std::mem::take(&mut inner.pending);
        inner.committed.extend(promoted);
    }

    /// Stage a `Read`'s fingerprint. It stays invisible to write-checks until
    /// the next [`Self::begin_response`] — i.e. until the model has seen the
    /// read's result on a later response. Overwrites any prior staged entry.
    pub(crate) fn record_read(&self, path: &Path, fingerprint: FileFingerprint) {
        let key = Self::key(path);
        self.inner.lock().pending.insert(key, fingerprint);
    }

    /// `stat` `path` and record its current fingerprint as **committed** —
    /// immediately visible to checks. Used by `Edit`/`Write` to re-anchor their
    /// own write: a later edit of the same file in the same response is editing
    /// the tool's own deterministic output, not blind to unseen content.
    /// Best-effort: a stat failure leaves the tracker unchanged.
    pub(crate) fn record_write_from_disk(&self, path: &Path) {
        if let Ok(meta) = std::fs::metadata(path) {
            self.record_committed(path, FileFingerprint::from_metadata(&meta));
        }
    }

    /// Insert directly into the committed set (already-seen reads: tool
    /// re-anchors and transcript hydration).
    fn record_committed(&self, path: &Path, fingerprint: FileFingerprint) {
        let key = Self::key(path);
        self.inner.lock().committed.insert(key, fingerprint);
    }

    /// Compare a committed read against the file's `current` fingerprint.
    /// The caller supplies `current` (it has just `stat`'d the file it is
    /// about to write), keeping this method free of I/O and trivially
    /// testable. Staged (same-response) reads are deliberately not consulted.
    pub(crate) fn check(&self, path: &Path, current: FileFingerprint) -> ReadCheck {
        let key = Self::key(path);
        match self.inner.lock().committed.get(&key) {
            None => ReadCheck::NeverRead,
            Some(recorded) if *recorded == current => ReadCheck::Current,
            Some(_) => ReadCheck::Stale,
        }
    }

    /// The fingerprint recorded for `path` (staged or committed), if any. The
    /// agent loop reads this right after a `Read` to stamp the value onto the
    /// persisted `ToolResult` row — the just-recorded read is still staged in
    /// `pending`, so both maps are consulted. `pub` because the caller is in
    /// `baybo-agent`.
    pub fn get(&self, path: &Path) -> Option<FileFingerprint> {
        let key = Self::key(path);
        let inner = self.inner.lock();
        inner
            .pending
            .get(&key)
            .or_else(|| inner.committed.get(&key))
            .copied()
    }

    /// Repopulate the tracker from a restored transcript: pair each anchoring
    /// `ToolUse` (for its `file_path`) with its `ToolResult` (for the fingerprint
    /// its [`ToolResultMeta`](baybo_model::ToolResultMeta) carried) and record
    /// it as committed. Later entries for the same file overwrite earlier ones,
    /// since messages are walked in order — so an `Edit` that followed a `Read`
    /// restores the post-write anchor, not the pre-write one. Getting that
    /// backwards told the model its own edit had changed the file behind its
    /// back, and cost a redundant re-read on the first turn after every restart.
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
                    && crate::READ_TRACKER_ANCHORING_TOOLS.contains(&name.as_str())
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
                    self.record_committed(Path::new(path), fp);
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
    fn pending_read_does_not_authorize_until_response_boundary() {
        // The P1 invariant: a read staged during the in-flight response cannot
        // authorize a write in that same response — only after `begin_response`
        // promotes it (i.e. once the model has seen the read's result).
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello").unwrap();
        let current = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());

        t.record_read(&p, current);
        assert_eq!(t.check(&p, current), ReadCheck::NeverRead);
        // `get` still sees the staged read (for stamping the persisted result).
        assert_eq!(t.get(&p), Some(current));

        t.begin_response();
        assert_eq!(t.check(&p, current), ReadCheck::Current);
    }

    #[test]
    fn size_change_is_detected_as_stale() {
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello").unwrap();
        t.record_read(
            &p,
            FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap()),
        );
        t.begin_response();
        // A different length flips the fingerprint regardless of mtime
        // resolution, so the check is deterministic.
        std::fs::write(&p, "hello world, much longer").unwrap();
        let now = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());
        assert_eq!(t.check(&p, now), ReadCheck::Stale);
    }

    #[test]
    fn write_reanchor_is_committed_immediately() {
        // A tool's own post-write re-anchor is visible to checks WITHOUT a
        // response boundary, so batched edits to one file in a single response
        // keep working.
        let t = ReadTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "one").unwrap();
        t.record_write_from_disk(&p);
        // Simulate a second write rewriting the file and re-anchoring afterwards.
        std::fs::write(&p, "two longer").unwrap();
        t.record_write_from_disk(&p);
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
        t.record_read(&p, current);
        t.begin_response();
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
        a.record_read(&p, current);
        a.begin_response();
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
                    approval: None,
                }),
            ),
        ];

        let t = ReadTracker::default();
        t.rebuild_from_messages(&messages);
        assert_eq!(t.check(&p, recorded), ReadCheck::Current);
    }

    /// An `Edit` re-anchors the tracker to what it wrote, so hydration has to
    /// restore *that*, not the `Read` that preceded it. Restoring the earlier
    /// one reports the model's own edit as a change behind its back, and the
    /// next write on that file is rejected until it re-reads.
    #[test]
    fn rebuild_restores_the_post_edit_anchor_not_the_pre_edit_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "before").unwrap();
        let read_fp = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());

        // The edit lands, moving the file's fingerprint.
        std::fs::write(&p, "after — a longer body").unwrap();
        let after_edit = FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());
        assert_ne!(read_fp, after_edit);

        let call = |id: &str, tool: &str| {
            ChatMessage::assistant(vec![ContentBlock::ToolUse {
                id: id.into(),
                name: tool.into(),
                input: json!({ "file_path": p.to_str().unwrap() }),
                signature: None,
            }])
        };
        let result = |id: &str, fp: FileFingerprint| {
            ChatMessage::tool_result_with_meta(
                id.into(),
                "ok".into(),
                Some(baybo_model::ToolResultMeta {
                    read_fingerprint: Some(fp),
                    approval: None,
                }),
            )
        };
        let messages = vec![
            call("call-1", crate::READ_TOOL_NAME),
            result("call-1", read_fp),
            call("call-2", crate::builtin::edit::EDIT_TOOL_NAME),
            result("call-2", after_edit),
        ];

        let t = ReadTracker::default();
        t.rebuild_from_messages(&messages);
        assert_eq!(
            t.check(&p, after_edit),
            ReadCheck::Current,
            "the post-edit anchor must win over the read that preceded it"
        );
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
