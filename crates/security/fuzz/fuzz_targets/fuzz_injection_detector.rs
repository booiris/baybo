#![no_main]

use std::sync::OnceLock;

use aura_security::{InjectionDetector, InjectionSeverity};
use libfuzzer_sys::fuzz_target;

static DETECTOR: OnceLock<InjectionDetector> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let detector = DETECTOR.get_or_init(InjectionDetector::with_default_rules);

    let warnings = detector.scan(text);

    for w in &warnings {
        assert!(
            w.location.start <= w.location.end,
            "empty or inverted range: {:?}",
            w.location
        );
        assert!(
            w.location.end <= text.len(),
            "location end {} out of bounds ({} bytes)",
            w.location.end,
            text.len()
        );
        assert!(
            text.is_char_boundary(w.location.start) && text.is_char_boundary(w.location.end),
            "match range not aligned to char boundary"
        );
    }

    let max = InjectionDetector::max_severity(&warnings);
    if warnings.is_empty() {
        assert!(max.is_none());
    } else {
        let observed = warnings.iter().map(|w| w.severity).max();
        assert_eq!(max, observed);
    }

    // Scanning twice must be deterministic.
    let again = detector.scan(text);
    assert_eq!(again.len(), warnings.len());

    // Null bytes always trip the critical-severity rule.
    if text.contains('\0') {
        assert_eq!(max, Some(InjectionSeverity::Critical));
    }
});
