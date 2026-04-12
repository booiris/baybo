use serde::{Deserialize, Serialize};

/// Trace collector configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TraceConfig {
    /// Whether to automatically snapshot context at intervals.
    pub auto_snapshot: bool,
    /// Number of spans between automatic snapshots.
    pub snapshot_interval: usize,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            auto_snapshot: true,
            snapshot_interval: 5,
        }
    }
}
