//! Persistence contract for the per-user pairing gate.
//!
//! Business logic (code minting, TTL semantics, refusal shape) lives
//! in `baybo-pairing`. This module keeps only the trait + row shape so
//! baybo-storage stays the single owner of every sqlite adapter. See
//! `docs/modules/pairing.md` for the end-to-end picture.

use async_trait::async_trait;
use baybo_model::ChannelType;

pub type Result<T> = std::result::Result<T, String>;

/// Lifecycle state of a pairing row. `Pending` rows carry an
/// `expires_at`; `Approved` rows clear it. An approved row has no
/// `expires_at` but is not retained forever: [`ChannelPairingStore::delete`]
/// removes it on demand, and the janitor's [`ChannelPairingStore::purge_expired`]
/// reaps it once `approved_at` ages past the retention TTL.
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
/// rows and is the handle the operator hands to `baybo pair approve`.
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
    /// never report as expired. This drives only the CLI `EXPIRED`
    /// badge; reaping (including TTL-aged approved rows) is decided by
    /// [`ChannelPairingStore::purge_expired`], not this helper.
    pub fn is_expired(&self, now: i64) -> bool {
        matches!(self.status, PairingStatus::Pending)
            && self.expires_at.is_some_and(|exp| now >= exp)
    }
}

/// Persistence contract for pairing rows.
#[async_trait]
pub trait ChannelPairingStore: Send + Sync {
    /// Return the row for `(channel_type, bot_id, user_id)`.
    async fn get(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<Option<ChannelPairingRow>>;

    /// Return the row that carries `code`.
    async fn get_by_code(&self, code: &str) -> Result<Option<ChannelPairingRow>>;

    /// Insert a pending row for the triple if none exists, or
    /// overwrite an expired pending row with the provided values.
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
    /// approved, or expired.
    async fn approve_by_code(&self, code: &str, now: i64) -> Result<Option<ChannelPairingRow>>;

    /// List rows, optionally filtered by status. Expired pending rows
    /// are still returned — the caller computes the `EXPIRED` badge
    /// against its own `now`.
    async fn list(&self, status: Option<PairingStatus>) -> Result<Vec<ChannelPairingRow>>;

    /// Hard-delete the row for the triple. No-op if already removed.
    async fn delete(&self, channel_type: &ChannelType, bot_id: &str, user_id: &str) -> Result<()>;

    /// Hard-delete pairing rows that the retention sweep should reap:
    ///   * `Pending` rows whose `expires_at <= now_secs`, and
    ///   * `Approved` rows whose `approved_at < cutoff_secs`.
    /// Both classes are auth-flow ephemera — pending codes are
    /// short-lived by nature, and an old approval is just a record
    /// of a one-time grant; live access is established at
    /// message-time via the channel route, not by retaining the
    /// approval row. Returns the number of rows removed.
    async fn purge_expired(&self, now_secs: i64, approved_cutoff_secs: i64) -> Result<u64>;
}
