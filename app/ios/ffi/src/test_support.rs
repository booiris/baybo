//! Host-only hooks for the netns matrix's phone process
//! (`examples/netns_phone.rs`). Behind the `test-support` feature, which no app
//! build enables.

use crate::binding::RELAY_MARKER;
use crate::keychain;
use crate::relay::pairing::PairedRecord;

/// Seed the in-memory keychain with a relay pairing, as `finish_pair` would
/// have persisted it. `record_json` is the keychain's own byte format.
pub fn seed_relay_pairing(record_json: &str) -> Result<(), String> {
    serde_json::from_str::<PairedRecord>(record_json)
        .map_err(|e| format!("decode paired record: {e}"))?;
    keychain::store_active_binding(RELAY_MARKER)?;
    keychain::store_paired_record(record_json.as_bytes())
}

/// Hold chat rotation off, or let it run again.
pub fn hold_chat_rotation(hold: bool) {
    crate::transport::ROTATION_HELD.store(hold, std::sync::atomic::Ordering::SeqCst);
}
