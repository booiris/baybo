//! The push role's device registry: `device_id → { apns_token, env,
//! gateway_pubkey, last_counter }` — the only per-device state it holds.
//!
//! It carries no conversation content — just the mapping from an opaque
//! `device_id` to its APNs token plus the material to authenticate binding
//! mutations.
//!
//! The store is keyed by `device_id` alone — a 32-byte Ed25519 public key, so
//! globally unique. The routes are keyless; isolation of one device's binding
//! from another's comes entirely from the **delegation chain**: a `/register`
//! carries the device's delegation + the gateway's signature, and a `/notify` is
//! verified against the `gateway_pubkey` stored at register. A `last_counter`
//! floor rejects replays.
//!
//! The map is **bounded**: a `/register` is a self-signed chain an attacker can
//! forge per request for a brand-new `device_id`, so the store mirrors the
//! rate-limiter maps' soft-cap + eviction. Each entry tracks a `last_seen` and
//! whether it has been **confirmed by a successful `/notify`**; over the cap, idle
//! unconfirmed bindings (the register-only flood) are evicted while a live device
//! is kept, and if nothing is evictable a new `/register` is refused outright.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::apns::ApnsEnv;

/// Soft cap on tracked device bindings before idle/unconfirmed entries are
/// evicted, mirroring the rate-limiter maps' bounded-growth pattern
/// (`ratelimit.rs` / `ip_limit.rs`). Bindings are longer-lived and more numerous
/// than transient rate buckets, so this sits above the limiter caps while still
/// bounding memory against a `/register` flood under the public `guest` key.
pub const DEVICE_STORE_SOFT_CAP: usize = 65_536;

/// How long a binding **never confirmed by a successful `/notify`** survives under
/// cap pressure. A real device is pushed to (→ confirmed) well within it; the
/// attacker's register-only floods age past it and are the first evicted when the
/// store is full. Confirmed bindings are exempt — a live device is never aged out;
/// a token that later dies is instead pruned on its next `/notify` `BadDeviceToken`.
pub const UNCONFIRMED_TTL: Duration = Duration::from_secs(3600);

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

/// Number of trailing `apns_token` characters retained when masking a device
/// binding for the operator dashboard; the rest of the secret never leaves the
/// push crate.
const DEVICE_TOKEN_MASK_TAIL: usize = 4;

/// Content-controlled view of a device binding for the operator dashboard. The
/// `apns_token` secret is masked to its last [`DEVICE_TOKEN_MASK_TAIL`] chars
/// **inside this crate**, so the raw token never crosses the crate boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    pub device_id: String,
    pub env: ApnsEnv,
    /// Last [`DEVICE_TOKEN_MASK_TAIL`] chars of the `apns_token` only.
    pub token_masked: String,
    pub gateway_pubkey: [u8; 32],
    pub last_counter: u64,
    pub confirmed: bool,
    /// Seconds elapsed since the binding was last seen (`Instant` can't cross the
    /// API, so it is resolved to an elapsed count here).
    pub idle_secs: u64,
}

/// `device_id → DeviceRegistration`. The push role's only per-device state.
pub trait DeviceTokenStore: Send + Sync {
    /// Bind (or replace) a device's registration; returns whether it was stored.
    /// `false` means the store is at its hard cap with no evictable (idle,
    /// unconfirmed) entry, so the caller refuses the `/register` with `503`. The
    /// caller verifies the delegation chain + replay counter first.
    fn register(&self, device_id: &str, reg: DeviceRegistration) -> bool;
    /// Resolve a device's current binding.
    fn get(&self, device_id: &str) -> Option<DeviceRegistration>;
    /// Unbind a device's token (on `400`/`410`). The device row on the gateway
    /// is never touched — only the APNs token mapping here.
    fn unbind(&self, device_id: &str);
    /// Mark a device's binding **confirmed by a successful `/notify`** (APNs
    /// accepted its token) and refresh its idle clock, so a live device is never
    /// aged out under cap pressure while an unconfirmed register-only flood is.
    fn confirm(&self, device_id: &str);
    /// Atomically advance the replay floor: stores `counter` and returns `true`
    /// iff it strictly exceeds the device's last accepted counter (else the
    /// caller rejects the request as a replay). A no-op `false` for an unknown
    /// device.
    fn advance_counter(&self, device_id: &str, counter: u64) -> bool;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Content-controlled snapshot of every binding for the operator dashboard,
    /// with the `apns_token` masked inside this crate.
    fn summaries(&self) -> Vec<DeviceSummary>;
}

/// One tracked binding plus its eviction metadata.
struct Entry {
    reg: DeviceRegistration,
    /// When this binding was registered or last confirmed by a successful
    /// `/notify`. The idle clock the TTL eviction reads.
    last_seen: Instant,
    /// A successful `/notify` delivery proved a real device behind this token, so
    /// it is exempt from idle eviction.
    confirmed: bool,
}

/// In-memory device-token store, keyed by `device_id`, bounded by a soft cap +
/// idle/unconfirmed eviction.
pub struct InMemoryDeviceTokenStore {
    inner: Mutex<HashMap<String, Entry>>,
    soft_cap: usize,
    unconfirmed_ttl: Duration,
}

impl Default for InMemoryDeviceTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryDeviceTokenStore {
    pub fn new() -> Self {
        Self::with_limits(DEVICE_STORE_SOFT_CAP, UNCONFIRMED_TTL)
    }

    /// Construct with explicit bounds (tests exercise eviction with a small cap).
    pub fn with_limits(soft_cap: usize, unconfirmed_ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            soft_cap,
            unconfirmed_ttl,
        }
    }

    fn register_at(&self, device_id: &str, reg: DeviceRegistration, now: Instant) -> bool {
        let mut inner = self.inner.lock();
        // Only a *new* device id can grow the map; a re-register replaces in place.
        if !inner.contains_key(device_id) && inner.len() >= self.soft_cap {
            let ttl = self.unconfirmed_ttl;
            // Shed idle, unconfirmed bindings (the register-only flood); keep live
            // devices and freshly-registered ones still inside the TTL.
            inner.retain(|_, e| e.confirmed || now.saturating_duration_since(e.last_seen) < ttl);
            if inner.len() >= self.soft_cap {
                // Nothing evictable — refuse rather than grow unbounded.
                return false;
            }
        }
        // Re-registering a confirmed device keeps it confirmed (a live device that
        // rebinds its token stays protected from eviction).
        let confirmed = inner.get(device_id).is_some_and(|e| e.confirmed);
        inner.insert(
            device_id.to_string(),
            Entry {
                reg,
                last_seen: now,
                confirmed,
            },
        );
        true
    }

    fn confirm_at(&self, device_id: &str, now: Instant) {
        if let Some(e) = self.inner.lock().get_mut(device_id) {
            e.confirmed = true;
            e.last_seen = now;
        }
    }
}

impl DeviceTokenStore for InMemoryDeviceTokenStore {
    fn register(&self, device_id: &str, reg: DeviceRegistration) -> bool {
        self.register_at(device_id, reg, Instant::now())
    }
    fn get(&self, device_id: &str) -> Option<DeviceRegistration> {
        self.inner.lock().get(device_id).map(|e| e.reg.clone())
    }
    fn unbind(&self, device_id: &str) {
        self.inner.lock().remove(device_id);
    }
    fn confirm(&self, device_id: &str) {
        self.confirm_at(device_id, Instant::now());
    }
    fn advance_counter(&self, device_id: &str, counter: u64) -> bool {
        let mut inner = self.inner.lock();
        match inner.get_mut(device_id) {
            Some(e) if counter > e.reg.last_counter => {
                e.reg.last_counter = counter;
                true
            }
            _ => false,
        }
    }
    fn len(&self) -> usize {
        self.inner.lock().len()
    }
    fn summaries(&self) -> Vec<DeviceSummary> {
        let now = Instant::now();
        self.inner
            .lock()
            .iter()
            .map(|(device_id, e)| {
                let chars = e.reg.apns_token.chars().count();
                let token_masked: String = e
                    .reg
                    .apns_token
                    .chars()
                    .skip(chars.saturating_sub(DEVICE_TOKEN_MASK_TAIL))
                    .collect();
                DeviceSummary {
                    device_id: device_id.clone(),
                    env: e.reg.env,
                    token_masked,
                    gateway_pubkey: e.reg.gateway_pubkey,
                    last_counter: e.reg.last_counter,
                    confirmed: e.confirmed,
                    idle_secs: now.saturating_duration_since(e.last_seen).as_secs(),
                }
            })
            .collect()
    }
}

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
        assert!(s.register("dev-1", reg("tok", 5)));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("dev-1").unwrap().apns_token, "tok");
        assert!(s.get("dev-2").is_none());
        s.unbind("dev-1");
        assert!(s.get("dev-1").is_none());
    }

    #[test]
    fn summaries_mask_the_apns_token_and_carry_metadata() {
        let s = InMemoryDeviceTokenStore::new();
        s.register("dev-1", reg("supersecrettoken", 9));
        let sums = s.summaries();
        assert_eq!(sums.len(), 1);
        let sum = &sums[0];
        assert_eq!(sum.device_id, "dev-1");
        assert_eq!(sum.last_counter, 9);
        assert_eq!(sum.gateway_pubkey, [7u8; 32]);
        // Only the last 4 chars survive; the raw secret never leaves the crate.
        assert_eq!(sum.token_masked, "oken");
        assert!(!sum.token_masked.contains("supersecret"));
    }

    #[test]
    fn summaries_short_token_is_not_padded() {
        let s = InMemoryDeviceTokenStore::new();
        s.register("dev-short", reg("ab", 1));
        let sums = s.summaries();
        assert_eq!(
            sums[0].token_masked, "ab",
            "a <4-char token masks to itself"
        );
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

    #[test]
    fn register_flood_stays_bounded_and_evicts_idle_unconfirmed() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(60);
        let s = InMemoryDeviceTokenStore::with_limits(3, ttl);
        // Fill the cap with unconfirmed registrations at t0.
        for i in 0..3 {
            assert!(s.register_at(&format!("d{i}"), reg("tok", 1), t0));
        }
        assert_eq!(s.len(), 3);
        // At the cap with every entry fresh (< TTL) and unconfirmed, nothing is
        // evictable, so a new device is refused — the store stays bounded.
        assert!(
            !s.register_at("d-new", reg("tok", 1), t0),
            "at cap, all fresh → refused"
        );
        assert_eq!(s.len(), 3);
        // Past the TTL the idle unconfirmed entries are evictable, so a new
        // registration sheds them and is admitted — never growing past the cap.
        let later = t0 + ttl + Duration::from_secs(1);
        assert!(s.register_at("d-new", reg("tok", 1), later));
        assert!(
            s.len() <= 3,
            "bounded at the soft cap under sustained churn"
        );
    }

    #[test]
    fn confirmed_devices_survive_eviction() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(60);
        let s = InMemoryDeviceTokenStore::with_limits(2, ttl);
        assert!(s.register_at("live", reg("tok", 1), t0));
        s.confirm_at("live", t0); // a successful /notify confirmed it
        assert!(s.register_at("idle", reg("tok", 1), t0));
        // Past the TTL, a new device triggers eviction: the confirmed "live" device
        // is kept; the idle unconfirmed one is shed to make room.
        let later = t0 + ttl + Duration::from_secs(1);
        assert!(s.register_at("new", reg("tok", 1), later));
        assert!(
            s.get("live").is_some(),
            "confirmed device survives eviction"
        );
        assert!(s.get("idle").is_none(), "idle unconfirmed device evicted");
        assert!(s.len() <= 2);
    }

    #[test]
    fn re_register_preserves_confirmed_and_does_not_grow() {
        let t0 = Instant::now();
        let s = InMemoryDeviceTokenStore::with_limits(4, Duration::from_secs(60));
        assert!(s.register_at("d", reg("a", 1), t0));
        s.confirm_at("d", t0);
        // Re-registering the same device replaces in place (no growth) and keeps it
        // confirmed, so it stays protected from eviction.
        assert!(s.register_at("d", reg("b", 2), t0));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("d").unwrap().apns_token, "b");
    }
}
