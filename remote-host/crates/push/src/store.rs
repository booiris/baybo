//! The push role's two registries: which gateways may use it (admission) and
//! `device_id → { apns_token, env }` (the only per-device state it holds).
//!
//! Neither carries conversation content — admission is machine-to-machine
//! instance keys, and the device store maps an opaque `device_id` to its APNs
//! token. The traits keep the in-memory impls (used here + in tests) swappable
//! for a durable backend without touching the `/notify` logic.

use parking_lot::Mutex;
use std::collections::HashMap;

use crate::apns::ApnsEnv;

/// A device's APNs binding, registered by its gateway (A) on the device's
/// behalf (gateway-mediated — the app never holds a C credential).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRegistration {
    pub apns_token: String,
    pub env: ApnsEnv,
}

/// `(instance_key, device_id) → { apns_token, env }`. The push role's only
/// per-device state. Keying by the **registering gateway's** instance key (not
/// `device_id` alone) keeps a remote host shared by multiple gateways
/// multi-tenant-safe: an admitted gateway can only register/notify/unbind its
/// own devices, never hijack or suppress another tenant's `device_id`.
pub trait DeviceTokenStore: Send + Sync {
    /// Register (or replace) a device's APNs binding, owned by `instance_key`.
    fn register(&self, instance_key: &str, device_id: &str, reg: DeviceRegistration);
    /// Resolve a device's current binding under its owning instance.
    fn get(&self, instance_key: &str, device_id: &str) -> Option<DeviceRegistration>;
    /// Unbind a device's token (on `400`/`410`). The device row on the gateway
    /// is never touched — only the APNs token mapping here.
    fn unbind(&self, instance_key: &str, device_id: &str);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory device-token store, partitioned by instance key.
#[derive(Default)]
pub struct InMemoryDeviceTokenStore {
    inner: Mutex<HashMap<String, HashMap<String, DeviceRegistration>>>,
}

impl InMemoryDeviceTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeviceTokenStore for InMemoryDeviceTokenStore {
    fn register(&self, instance_key: &str, device_id: &str, reg: DeviceRegistration) {
        self.inner
            .lock()
            .entry(instance_key.to_string())
            .or_default()
            .insert(device_id.to_string(), reg);
    }
    fn get(&self, instance_key: &str, device_id: &str) -> Option<DeviceRegistration> {
        self.inner
            .lock()
            .get(instance_key)
            .and_then(|m| m.get(device_id).cloned())
    }
    fn unbind(&self, instance_key: &str, device_id: &str) {
        let mut inner = self.inner.lock();
        if let Some(devices) = inner.get_mut(instance_key) {
            devices.remove(device_id);
            if devices.is_empty() {
                inner.remove(instance_key);
            }
        }
    }
    fn len(&self) -> usize {
        self.inner.lock().values().map(HashMap::len).sum()
    }
}

/// Per-instance admission (the gateway allow-list) lives in the shared crate, so
/// the relay and push roles check the same live, hot-reloaded list.
pub use remote_host_admission::{Admission, InMemoryAdmission};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_store_register_get_unbind() {
        let s = InMemoryDeviceTokenStore::new();
        assert!(s.is_empty());
        s.register(
            "inst-1",
            "dev-1",
            DeviceRegistration {
                apns_token: "tok".into(),
                env: ApnsEnv::Sandbox,
            },
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("inst-1", "dev-1").unwrap().env, ApnsEnv::Sandbox);
        // A different instance can't see another tenant's device under the same id.
        assert!(s.get("inst-2", "dev-1").is_none());
        s.unbind("inst-1", "dev-1");
        assert!(s.get("inst-1", "dev-1").is_none());
    }
}
