use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Fingerprint of a file captured at the moment it was read.
///
/// `mtime` + `size` is a cheap, metadata-only change signal: any in-place
/// modification (a user edit, a formatter/linter, `git checkout`) moves the
/// mtime, and a content change of a different length also moves the size.
/// Together they catch the realistic ways a file diverges from what was last
/// read, without re-hashing the whole file.
///
/// Lives in `baybo-model` because it rides along with
/// [`crate::ContentBlock::ToolResult`] (so a `Read` result persists the
/// fingerprint of what it read) and is also the value type the read-before-write
/// tracker in `baybo-tools` keys on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub mtime: SystemTime,
    pub size: u64,
}

impl FileFingerprint {
    pub fn new(mtime: SystemTime, size: u64) -> Self {
        Self { mtime, size }
    }

    /// Build a fingerprint from an already-`stat`'d file. The mtime falls back
    /// to the epoch on the rare platform/filesystem that can't report one (then
    /// comparison degrades to size-only) — Baybo is Unix-only, so in practice
    /// `modified()` always succeeds. Pure: reads the handed-in `Metadata`, does
    /// no I/O.
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
        }
    }
}
