//! Persistence contract for the iOS-companion **device registry**.
//!
//! Distinct from the in-memory channel token table: device credentials must
//! survive a gateway restart, so they live in libsql. The gateway binds to at
//! most **one** device — one gateway = one app. Rows are keyed by `device_id`
//! (minted fresh per pairing, but stable per physical device), and at most one
//! row is ever `Approved` at a time: re-pairing supersedes the prior binding
//! rather than accumulating a second live device. The invariant is held
//! transactionally by [`DeviceStore::create_replacing_approved`] and backstopped
//! by the `idx_devices_one_approved` partial unique index.
//!
//! Lifecycle: a row is written `Approved` the moment the phone user and the
//! operator both confirm the pairing (carrying the device's static pubkey + an
//! active `auth_token`), and flipped to `Revoked` on revoke — or when a newer
//! pairing replaces it. Per the project rule, **revoke never deletes the row**
//! — it keeps the audit trail (so a superseded device stays visible in
//! `device list`) and stops the `auth_token` UNIQUE slot from being silently
//! reused. The APNs token does not live here (it lives in the remote-host push
//! store); A only needs `device_id` to address a push.
//!
//! Business logic (the XXpsk0 handshake, rendezvous/secret minting, TTL) lives
//! in `baybo-pairing`; this module is only the trait + row shape, keeping
//! `baybo-storage` the single owner of every libsql adapter.

use async_trait::async_trait;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Lifecycle state of a device row. A row exists only once both ends confirmed
/// the pairing, so there is no inert/pending state — it is `Approved` from
/// creation until `Revoked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Active — the `auth_token` authenticates the scoped device surface
    /// (`/v1/chat/*` + channel-ws).
    Approved,
    /// Revoked — the row (and its `auth_token` slot) is retained for audit but
    /// the token no longer authenticates anything.
    Revoked,
}

impl DeviceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceStatus::Approved => "approved",
            DeviceStatus::Revoked => "revoked",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "approved" => Some(Self::Approved),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// One device-registry row. Natural key is `device_id`; `auth_token` is unique
/// across all rows (live and revoked).
#[derive(Debug, Clone)]
pub struct DeviceRow {
    /// Client-generated and **stable per physical device**: the app keys it off
    /// a keychain-pinned Noise static, so re-pairing the same phone yields the
    /// same id. Re-pairing the same device updates its row in place
    /// (`create_replacing_approved` upserts on the natural key); pairing a
    /// *different* device supersedes the prior one, whose row is revoked (kept
    /// for audit), never reused.
    pub device_id: String,
    /// The device's X25519 static public key, exchanged at pairing (32 bytes).
    pub device_pubkey: Vec<u8>,
    /// 256-bit hex bearer for the scoped REST/WS surface. Active from creation
    /// (the row only exists once both ends confirmed).
    pub auth_token: String,
    pub status: DeviceStatus,
    /// The **public** rendezvous id this device paired under, retained for the
    /// operator's device list / audit. Never the QR secret (that stays in the
    /// in-memory slot and is zeroized on consume). `None` if cleared.
    pub rendezvous_id: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; set at pairing-confirm time (when the row is created).
    pub approved_at: Option<i64>,
    /// Unix seconds; bumped on device activity.
    pub last_seen_at: Option<i64>,
}

/// Persistence contract for device-registry rows.
#[async_trait]
pub trait DeviceStore: Send + Sync {
    /// Insert a freshly-paired (`Approved`) device row. Errors with
    /// [`StorageError::Conflict`] if the `device_id` or the `auth_token` already
    /// exists. Does **not** enforce the one-approved invariant on its own — the
    /// pairing path uses
    /// [`create_replacing_approved`](Self::create_replacing_approved); plain
    /// `create` is the bare insert primitive (tests, fixtures).
    async fn create(&self, row: &DeviceRow) -> Result<()>;

    /// Atomically supersede the current binding with a freshly-paired device: in
    /// one transaction, revoke every still-`Approved` row, then upsert `row`
    /// (itself `Approved`). Because `device_id` is a stable per-device identity,
    /// re-pairing the **same** device refreshes its row in place (new
    /// token/pubkey) instead of colliding on the natural key; pairing a
    /// **different** device inserts a fresh row and leaves the prior one revoked
    /// (kept for audit). Returns the `device_id`s of any *other* bindings
    /// superseded — empty on a first pairing or a same-device re-pair — so the
    /// caller can report what was replaced. This is the write path that holds the
    /// **one approved device** (1:1 gateway↔app) invariant; the
    /// `idx_devices_one_approved` partial unique index is its backstop. Errors
    /// with [`StorageError::Conflict`] only on a genuine `auth_token` collision.
    async fn create_replacing_approved(&self, row: &DeviceRow) -> Result<Vec<String>>;

    /// Fetch one row by its natural key.
    async fn get(&self, device_id: &str) -> Result<Option<DeviceRow>>;

    /// Resolve an **approved** device by its bearer token — the gateway auth
    /// path. Revoked rows never match, so the security-critical status filter
    /// lives in one place (the SQL), not at every call site.
    async fn lookup_approved_by_auth_token(&self, auth_token: &str) -> Result<Option<DeviceRow>>;

    /// Resolve an **approved** device by its X25519 static public key — the
    /// relay content path, where no bearer token is presented (the gateway
    /// dials the relay blind and the Noise IK initiator's static is the only
    /// identity it learns). The static-key authentication of the device hangs
    /// off this match, so the `approved`-only filter is again security-critical
    /// and lives in the query. Revoked rows never match.
    async fn lookup_approved_by_pubkey(&self, device_pubkey: &[u8]) -> Result<Option<DeviceRow>>;

    /// List all rows, optionally filtered by status. Newest `created_at` first.
    /// The push dispatcher uses the `Approved` filter to fan a turn-completion
    /// preview to every approved device.
    async fn list(&self, status: Option<DeviceStatus>) -> Result<Vec<DeviceRow>>;

    /// Flip a row to `Revoked` (keeping the row + token slot). Returns `true`
    /// if a row changed, `false` if it was unknown or already revoked.
    async fn revoke(&self, device_id: &str) -> Result<bool>;

    /// Bump `last_seen_at` on device activity. No-op if the row is gone.
    async fn touch_last_seen(&self, device_id: &str, now: i64) -> Result<()>;
}
