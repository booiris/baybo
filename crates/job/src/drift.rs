//! Soul-version drift records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One observation of soul-version drift between session bind time and
/// job start time.
///
/// The session's `bound_soul_version` is locked at session creation. If
/// a hot-reload changes the active soul version mid-session, the next
/// job records the divergence here. Empty `provenance_drift` on a job
/// means "session contract held when this job ran".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftRecord {
    /// What the job actually loaded.
    pub effective_soul_version: String,
    /// What the session was bound to at creation.
    pub expected_soul_version: String,
    /// When the divergence was observed (job start time).
    pub observed_at: DateTime<Utc>,
    /// Free-text annotation — usually populated by the loader to name
    /// the cause (`"hot reload by config watcher"`, `"manual reload via
    /// /reload-soul"`, …).
    pub note: Option<String>,
}
