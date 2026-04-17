#![no_main]

use std::sync::OnceLock;

use aura_security::{EncryptionKey, PlaceholderMinter};
use libfuzzer_sys::fuzz_target;

const FIXED_KEY: [u8; 32] = *b"aura-fuzz-placeholder-master-key";

static MINTER: OnceLock<PlaceholderMinter> = OnceLock::new();

fn minter() -> &'static PlaceholderMinter {
    MINTER.get_or_init(|| {
        let key = EncryptionKey::new(FIXED_KEY.to_vec()).expect("32-byte key");
        PlaceholderMinter::from_master_key(&key)
    })
}

fuzz_target!(|data: &[u8]| {
    let minter = minter();

    let ph = minter.mint(data);
    assert!(
        ph.starts_with("{{SECRET_") && ph.ends_with("}}"),
        "unexpected placeholder shape: {ph}"
    );
    // "{{SECRET_" (9) + 24 hex chars + "}}" (2) = 35 bytes.
    assert_eq!(ph.len(), 35);

    let re = PlaceholderMinter::placeholder_regex();
    assert!(
        re.is_match(&ph),
        "regex did not match minted placeholder {ph}"
    );

    // Determinism within a single minter.
    let again = minter.mint(data);
    assert_eq!(ph, again);
});
