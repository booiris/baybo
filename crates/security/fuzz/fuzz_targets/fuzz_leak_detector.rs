#![no_main]

use std::sync::OnceLock;

use baybo_security::LeakDetector;
use libfuzzer_sys::fuzz_target;

static DETECTOR: OnceLock<LeakDetector> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let detector = DETECTOR.get_or_init(LeakDetector::with_default_rules);

    let result = detector.scan_text(text);

    assert_eq!(result.blocked, result.block_reason.is_some());

    for m in &result.matches {
        assert!(!m.original.is_empty(), "empty match emitted");
        assert!(
            text.contains(m.original.as_str()),
            "match '{}' not a substring of input",
            m.original
        );
        assert!(!m.rule_name.is_empty());
    }

    // Deterministic: scanning twice yields the same tallies.
    let again = detector.scan_text(text);
    assert_eq!(again.matches.len(), result.matches.len());
    assert_eq!(again.blocked, result.blocked);
});
