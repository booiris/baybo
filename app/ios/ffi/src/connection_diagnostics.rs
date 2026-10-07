use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::api::{ConnectionLogEntry, ConnectionLogStage};

pub(crate) const MAX_ENTRIES: usize = 500;

static CAPTURE: Mutex<Capture> = Mutex::new(Capture {
    enabled: false,
    sequence: 0,
    entries: VecDeque::new(),
});

struct Capture {
    enabled: bool,
    sequence: u64,
    entries: VecDeque<ConnectionLogEntry>,
}

impl Capture {
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.entries.clear();
    }

    fn push(&mut self, stage: ConnectionLogStage, message: String) {
        if !self.enabled {
            return;
        }
        self.sequence += 1;
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(ConnectionLogEntry {
            sequence: self.sequence,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            stage,
            message,
        });
    }
}

pub(crate) fn set_enabled(enabled: bool) {
    let mut capture = CAPTURE.lock();
    capture.set_enabled(enabled);
    capture.push(
        ConnectionLogStage::Lifecycle,
        "Connection diagnostics started".into(),
    );
}

// Dedicated events avoid copying credentials, response bodies or chat frames
// from general-purpose logs into the exportable connection console.
pub(crate) fn record(stage: ConnectionLogStage, message: impl Into<String>) {
    CAPTURE.lock().push(stage, message.into());
}

pub(crate) fn drain() -> Vec<ConnectionLogEntry> {
    CAPTURE.lock().entries.drain(..).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_opt_in_bounded_and_resets_between_visits() {
        let mut capture = Capture {
            enabled: false,
            sequence: 0,
            entries: VecDeque::new(),
        };
        capture.push(ConnectionLogStage::Chat, "disabled".into());
        assert!(capture.entries.is_empty());
        capture.set_enabled(true);
        for i in 0..MAX_ENTRIES + 3 {
            capture.push(ConnectionLogStage::Probe, i.to_string());
        }
        assert_eq!(capture.entries.len(), MAX_ENTRIES);
        assert_eq!(capture.entries.front().unwrap().message, "3");
        assert_eq!(
            capture.entries.back().unwrap().sequence,
            (MAX_ENTRIES + 3) as u64
        );
        capture.set_enabled(false);
        capture.push(ConnectionLogStage::Chat, "disabled again".into());
        assert!(capture.entries.is_empty());
        capture.set_enabled(true);
        capture.push(ConnectionLogStage::Network, "new visit".into());
        assert_eq!(capture.entries.len(), 1);
        assert_eq!(capture.entries.front().unwrap().message, "new visit");
    }
}
