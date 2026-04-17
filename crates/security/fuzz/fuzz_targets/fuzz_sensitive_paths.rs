#![no_main]

use std::path::Path;

use aura_security::is_sensitive_path;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };

    // `is_sensitive_path` canonicalizes, which on some platforms blocks
    // truly pathological inputs. We only care that it never panics and
    // yields a consistent verdict on repeat calls.
    let path = Path::new(text);
    let a = is_sensitive_path(path);
    let b = is_sensitive_path(path);
    assert_eq!(a, b, "verdict flipped between two consecutive calls");
});
