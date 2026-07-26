#![no_main]

use baybo_security::EncryptionKey;
use baybo_security::crypto::{decrypt, encrypt};
use libfuzzer_sys::fuzz_target;

const KEY_LEN: usize = 32;

fuzz_target!(|data: &[u8]| {
    if data.len() < KEY_LEN + 1 {
        return;
    }
    let mode = data[0];
    let (key_bytes, payload) = data[1..].split_at(KEY_LEN);

    let key = EncryptionKey::new(key_bytes.to_vec()).expect("32 bytes is valid");

    if mode & 1 == 0 {
        // Roundtrip: encrypt then decrypt must recover the payload byte-for-byte.
        let ciphertext = encrypt(payload, &key, b"fuzz.entry").expect("encrypt of arbitrary bytes succeeds");
        assert!(
            ciphertext.len() >= 12 + 16 + payload.len(),
            "ciphertext shorter than nonce+tag+payload"
        );
        let plaintext = decrypt(&ciphertext, &key, b"fuzz.entry").expect("decrypt of own ciphertext succeeds");
        assert_eq!(plaintext, payload);
    } else {
        // Decrypt-only: feeding arbitrary bytes as ciphertext must never
        // panic. Authentication failure is the expected happy path.
        let _ = decrypt(payload, &key, b"fuzz.entry");
    }
});
