//! Persistence contract for the per-user pairing gate.
//!
//! Business logic (code minting, TTL semantics, refusal shape) lives
//! in `aura-pairing`. This module keeps only the trait + row shape so
//! aura-storage stays the single owner of every libsql adapter. See
//! `docs/modules/pairing.md` for the end-to-end picture.

use async_trait::async_trait;
use aura_model::ChannelType;

pub type Result<T> = std::result::Result<T, String>;

/// Lifecycle state of a pairing row. `Pending` rows carry an
/// `expires_at`; `Approved` rows clear it — approved pairings don't
/// auto-expire, they stay live until [`ChannelPairingStore::delete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStatus {
    Pending,
    Approved,
}

impl PairingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PairingStatus::Pending => "pending",
            PairingStatus::Approved => "approved",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            _ => None,
        }
    }
}

/// One pairing row. The natural key is the triple
/// `(channel_type, bot_id, user_id)`; `code` is unique across live
/// rows and is the handle the operator hands to `aura pair approve`.
#[derive(Debug, Clone)]
pub struct ChannelPairingRow {
    pub channel_type: ChannelType,
    pub bot_id: String,
    pub user_id: String,
    pub code: String,
    pub status: PairingStatus,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds. `None` once the row is approved.
    pub expires_at: Option<i64>,
    /// Unix seconds.
    pub approved_at: Option<i64>,
}

impl ChannelPairingRow {
    /// `true` when the row is `Pending` and `now >= expires_at`.
    /// Approved rows and pending rows without an `expires_at` stamp
    /// never report as expired.
    pub fn is_expired(&self, now: i64) -> bool {
        matches!(self.status, PairingStatus::Pending)
            && self.expires_at.is_some_and(|exp| now >= exp)
    }
}

/// Persistence contract for pairing rows. Implementations treat the
/// natural key `(channel_type, bot_id, user_id)` as soft-deletable —
/// `delete` sets a tombstone, `upsert_pending` revives it with fresh
/// state.
#[async_trait]
pub trait ChannelPairingStore: Send + Sync {
    /// Return the live row for `(channel_type, bot_id, user_id)`.
    async fn get(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<Option<ChannelPairingRow>>;

    /// Return the live row that carries `code`.
    async fn get_by_code(&self, code: &str) -> Result<Option<ChannelPairingRow>>;

    /// Insert a pending row for the triple if no live row exists, or
    /// overwrite an expired / tombstoned row with the provided values.
    /// A live non-expired row wins on conflict (the existing code
    /// survives so concurrent inbounds from the same user agree).
    /// `now` and `expires_at` are Unix seconds.
    ///
    /// Returns the canonical row — which may be the pre-existing one
    /// if a racer beat this caller to the insert.
    async fn upsert_pending(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
        code: &str,
        now: i64,
        expires_at: i64,
    ) -> Result<ChannelPairingRow>;

    /// Flip a pending row identified by `code` to approved. Returns
    /// the updated row, or `None` if the code is unknown, already
    /// approved, expired, or revoked.
    async fn approve_by_code(&self, code: &str, now: i64) -> Result<Option<ChannelPairingRow>>;

    /// List live rows, optionally filtered by status. Expired pending
    /// rows are still returned — the caller computes the `EXPIRED`
    /// badge against its own `now`.
    async fn list(&self, status: Option<PairingStatus>) -> Result<Vec<ChannelPairingRow>>;

    /// Soft-delete the row for the triple. No-op if already tombstoned.
    async fn delete(&self, channel_type: &ChannelType, bot_id: &str, user_id: &str) -> Result<()>;
}
