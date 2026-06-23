//! Persistence contract for the iOS-companion **device registry**.
//!
//! Distinct from the in-memory channel token table: device credentials must
//! survive a gateway restart, so they live in libsql. One row per
//! `(user_id, device_id)` binding — `device_id` is per-pairing (multi-gateway),
//! so a single phone that pairs with home + work gateways lands two rows in
//! two gateways' tables.
//!
//! Lifecycle: a row is written `Pending` when SPAKE2 pairing completes
//! (carrying the device's static pubkey + an inert `auth_token`), activated by
//! `aura device approve <code>`, and flipped to `Revoked` on revoke. Per the
//! project rule, **revoke never deletes the row** — it keeps the audit trail
//! and stops the `auth_token` UNIQUE slot from being silently reused. The APNs
//! token does not live here (it lives in the remote-host push store); A only
//! needs `device_id` to address a push.
//!
//! Business logic (SPAKE2, code minting, TTL) lives in `aura-pairing`; this
//! module is only the trait + row shape, keeping `aura-storage` the single
//! owner of every libsql adapter.

use async_trait::async_trait;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Lifecycle state of a device row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Paired but not yet operator-approved — the `auth_token` is inert.
    Pending,
    /// Operator-approved — the `auth_token` authenticates the scoped device
    /// surface (`/v1/chat/*` + channel-ws).
    Approved,
    /// Revoked — the row (and its `auth_token` slot) is retained for audit but
    /// the token no longer authenticates anything.
    Revoked,
}

impl DeviceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceStatus::Pending => "pending",
            DeviceStatus::Approved => "approved",
            DeviceStatus::Revoked => "revoked",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// One device-registry row. Natural key is `(user_id, device_id)`;
/// `auth_token` is unique across all rows (live and revoked).
#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub user_id: String,
    /// Client-generated, per-pairing (multi-gateway).
    pub device_id: String,
    /// Human label for the device list ("Booiris iPhone").
    pub label: String,
    /// The device's X25519 static public key, exchanged at pairing (32 bytes).
    pub device_pubkey: Vec<u8>,
    /// 256-bit hex bearer for the scoped REST/WS surface. Inert until approved.
    pub auth_token: String,
    pub status: DeviceStatus,
    /// The retained SPAKE2 code, used as the operator approval handle
    /// (`aura device approve <code>`). `None` once consumed/cleared.
    pub pairing_code: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; set when the operator approves.
    pub approved_at: Option<i64>,
    /// Unix seconds; bumped on device activity.
    pub last_seen_at: Option<i64>,
}

/// Persistence contract for device-registry rows.
#[async_trait]
pub trait DeviceStore: Send + Sync {
    /// Insert a freshly-paired (`Pending`) device row. Errors with
    /// [`StorageError::Conflict`] if the `(user_id, device_id)` or the
    /// `auth_token` already exists.
    async fn create(&self, row: &DeviceRow) -> Result<()>;

    /// Fetch one row by its natural key.
    async fn get(&self, user_id: &str, device_id: &str) -> Result<Option<DeviceRow>>;

    /// Resolve an **approved** device by its bearer token — the gateway auth
    /// path. Pending and revoked rows never match, so the security-critical
    /// status filter lives in one place (the SQL), not at every call site.
    async fn lookup_approved_by_auth_token(&self, auth_token: &str) -> Result<Option<DeviceRow>>;

    /// List all rows, optionally filtered by status. Newest `created_at` first.
    async fn list(&self, status: Option<DeviceStatus>) -> Result<Vec<DeviceRow>>;

    /// List one user's rows, optionally filtered by status. Used by the push
    /// dispatcher to fan a turn-completion preview to that user's **approved**
    /// devices. Newest `created_at` first.
    async fn list_for_user(
        &self,
        user_id: &str,
        status: Option<DeviceStatus>,
    ) -> Result<Vec<DeviceRow>>;

    /// Flip a `Pending` row identified by `pairing_code` to `Approved`,
    /// stamping `approved_at = now`. Returns the updated row, or `None` if the
    /// code is unknown or the row is not pending.
    async fn approve_by_code(&self, code: &str, now: i64) -> Result<Option<DeviceRow>>;

    /// Flip a row to `Revoked` (keeping the row + token slot). Returns `true`
    /// if a row changed, `false` if it was unknown or already revoked.
    async fn revoke(&self, user_id: &str, device_id: &str) -> Result<bool>;

    /// Bump `last_seen_at` on device activity. No-op if the row is gone.
    async fn touch_last_seen(&self, user_id: &str, device_id: &str, now: i64) -> Result<()>;
}
