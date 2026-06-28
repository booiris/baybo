//! The push role's two registries: which gateways may use it (admission) and
//! `device_id → { apns_token, env, gateway_pubkey, last_counter }` (the only
//! per-device state it holds).
//!
//! Neither carries conversation content — admission is machine-to-machine
//! `remote_api_key`s, and the device store maps an opaque `device_id` to its APNs
//! token plus the material to authenticate binding mutations.
//!
//! The store is keyed by `device_id` alone — a 32-byte Ed25519 public key, so
//! globally unique. Isolation of one device's binding from another's no longer
//! comes from the `remote_api_key` partition (a shared `guest` key offers none)
//! but from the **delegation chain**: a `/register` carries the device's
//! delegation + the gateway's signature, and a `/notify` is verified against the
//! `gateway_pubkey` stored at register. A `last_counter` floor rejects replays.

use parking_lot::Mutex;
use std::collections::HashMap;

use crate::apns::ApnsEnv;

/// A device's APNs binding plus the material authenticating its mutations.
/// Registered by the device's delegated gateway (A) on the device's behalf
/// (gateway-mediated — the app never holds APNs provider credentials).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRegistration {
    pub apns_token: String,
    pub env: ApnsEnv,
    /// The gateway Ed25519 push key C verified the device's delegation against at
    /// `/register`. `/notify` signatures are checked against it.
    pub gateway_pubkey: [u8; 32],
    /// Highest replay counter accepted for this device; a `/register` or
    /// `/notify` must strictly exceed it.
    pub last_counter: u64,
}

/// `device_id → DeviceRegistration`. The push role's only per-device state.
pub trait DeviceTokenStore: Send + Sync {
    /// Bind (or replace) a device's registration. The caller verifies the
    /// delegation chain + replay counter first.
    fn register(&self, device_id: &str, reg: DeviceRegistration);
    /// Resolve a device's current binding.
    fn get(&self, device_id: &str) -> Option<DeviceRegistration>;
    /// Unbind a device's token (on `400`/`410`). The device row on the gateway
    /// is never touched — only the APNs token mapping here.
    fn unbind(&self, device_id: &str);
    /// Atomically advance the replay floor: stores `counter` and returns `true`
    /// iff it strictly exceeds the device's last accepted counter (else the
    /// caller rejects the request as a replay). A no-op `false` for an unknown
    /// device.
    fn advance_counter(&self, device_id: &str, counter: u64) -> bool;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory device-token store, keyed by `device_id`.
#[derive(Default)]
pub struct InMemoryDeviceTokenStore {
    inner: Mutex<HashMap<String, DeviceRegistration>>,
}

impl InMemoryDeviceTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeviceTokenStore for InMemoryDeviceTokenStore {
    fn register(&self, device_id: &str, reg: DeviceRegistration) {
        self.inner.lock().insert(device_id.to_string(), reg);
    }
    fn get(&self, device_id: &str) -> Option<DeviceRegistration> {
        self.inner.lock().get(device_id).cloned()
    }
    fn unbind(&self, device_id: &str) {
        self.inner.lock().remove(device_id);
    }
    fn advance_counter(&self, device_id: &str, counter: u64) -> bool {
        let mut inner = self.inner.lock();
        match inner.get_mut(device_id) {
            Some(reg) if counter > reg.last_counter => {
                reg.last_counter = counter;
                true
            }
            _ => false,
        }
    }
    fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

/// Per-key admission (the gateway allow-list) lives in the shared crate, so the
/// relay and push roles resolve the same live, hot-reloaded list.
pub use remote_host_admission::{Admission, Admit, InMemoryAdmission};

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(token: &str, counter: u64) -> DeviceRegistration {
        DeviceRegistration {
            apns_token: token.into(),
            env: ApnsEnv::Sandbox,
            gateway_pubkey: [7u8; 32],
            last_counter: counter,
        }
    }

    #[test]
    fn register_get_unbind() {
        let s = InMemoryDeviceTokenStore::new();
        assert!(s.is_empty());
        s.register("dev-1", reg("tok", 5));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("dev-1").unwrap().apns_token, "tok");
        assert!(s.get("dev-2").is_none());
        s.unbind("dev-1");
        assert!(s.get("dev-1").is_none());
    }

    #[test]
    fn advance_counter_rejects_non_increasing() {
        let s = InMemoryDeviceTokenStore::new();
        s.register("dev-1", reg("tok", 10));
        assert!(!s.advance_counter("dev-1", 10), "equal is a replay");
        assert!(!s.advance_counter("dev-1", 9), "older is a replay");
        assert!(s.advance_counter("dev-1", 11), "newer advances");
        assert_eq!(s.get("dev-1").unwrap().last_counter, 11);
        // Unknown device: no bucket to advance.
        assert!(!s.advance_counter("ghost", 1));
    }
}
