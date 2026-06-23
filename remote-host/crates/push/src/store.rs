//! The push role's two registries: which gateways may use it (admission) and
//! `device_id → { apns_token, env }` (the only per-device state it holds).
//!
//! Neither carries conversation content — admission is machine-to-machine
//! instance keys, and the device store maps an opaque `device_id` to its APNs
//! token. The traits keep the in-memory impls (used here + in tests) swappable
//! for a durable backend without touching the `/notify` logic.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};

use crate::apns::ApnsEnv;

/// A device's APNs binding, registered by its gateway (A) on the device's
/// behalf (gateway-mediated — the app never holds a C credential).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRegistration {
    pub apns_token: String,
    pub env: ApnsEnv,
}

/// `device_id → { apns_token, env }`. The push role's only per-device state.
pub trait DeviceTokenStore: Send + Sync {
    /// Register (or replace) a device's APNs binding.
    fn register(&self, device_id: &str, reg: DeviceRegistration);
    /// Resolve a device's current binding.
    fn get(&self, device_id: &str) -> Option<DeviceRegistration>;
    /// Unbind a device's token (on `400`/`410`). The device row on the gateway
    /// is never touched — only the APNs token mapping here.
    fn unbind(&self, device_id: &str);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory device-token store.
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
    fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

/// "Auth" on the push role = per-instance admission only (machine-to-machine,
/// the Bitwarden installation-id model). Never device auth, never plaintext.
pub trait Admission: Send + Sync {
    fn is_admitted(&self, instance_key: &str) -> bool;
}

/// In-memory admission allow-list of valid per-instance keys.
#[derive(Default)]
pub struct InMemoryAdmission {
    keys: Mutex<HashSet<String>>,
}

impl InMemoryAdmission {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            keys: Mutex::new(keys.into_iter().map(Into::into).collect()),
        }
    }

    pub fn admit(&self, instance_key: impl Into<String>) {
        self.keys.lock().insert(instance_key.into());
    }

    pub fn revoke(&self, instance_key: &str) {
        self.keys.lock().remove(instance_key);
    }
}

impl Admission for InMemoryAdmission {
    fn is_admitted(&self, instance_key: &str) -> bool {
        self.keys.lock().contains(instance_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_store_register_get_unbind() {
        let s = InMemoryDeviceTokenStore::new();
        assert!(s.is_empty());
        s.register(
            "dev-1",
            DeviceRegistration {
                apns_token: "tok".into(),
                env: ApnsEnv::Sandbox,
            },
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("dev-1").unwrap().env, ApnsEnv::Sandbox);
        s.unbind("dev-1");
        assert!(s.get("dev-1").is_none());
    }

    #[test]
    fn admission_allow_list() {
        let a = InMemoryAdmission::with_keys(["inst-A"]);
        assert!(a.is_admitted("inst-A"));
        assert!(!a.is_admitted("inst-B"));
        a.admit("inst-B");
        assert!(a.is_admitted("inst-B"));
        a.revoke("inst-A");
        assert!(!a.is_admitted("inst-A"));
    }
}
