//! Persistence interface for deck cards and their data snapshots.
//!
//! A deck card is an agent-authored bundle on disk (`workspace/deck/<uuid>/`);
//! the row here is the **authoritative** runtime state (layout, enable,
//! quarantine, soft-delete) — manifest values are install-time defaults only.
//! Snapshots are ephemeral render state: the sqlite impl prunes to a small
//! latest-N per card on insert, so nothing here is transcript-grade data.
//! `last_seq` lives on the card row, not the snapshot table, so the push
//! counter can never regress when snapshots are pruned.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Card tile size class on the deck's 2-column grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckSize {
    /// 1×1 tile.
    Small,
    /// 2×1 full-width tile.
    Wide,
    /// 2×2 full-width double-height tile.
    Large,
}

impl DeckSize {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeckSize::Small => "small",
            DeckSize::Wide => "wide",
            DeckSize::Large => "large",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "small" => Some(DeckSize::Small),
            "wide" => Some(DeckSize::Wide),
            "large" => Some(DeckSize::Large),
            _ => None,
        }
    }
}

/// One row of `deck_cards`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckCardRow {
    /// Card uuid — also the bundle directory name under `workspace/deck/`.
    pub id: String,
    pub title: String,
    /// Manual order on the deck, ascending.
    pub position: i64,
    pub size: DeckSize,
    pub enabled: bool,
    /// Set when the runtime auto-disabled the card (crash/timeout window).
    pub quarantined_at: Option<DateTime<Utc>>,
    /// Soft-delete tombstone; live listings filter on `IS NULL` in SQL.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Content hash of the installed bundle's agent-written files.
    pub spec_hash: String,
    /// Monotonic push counter; assigned at emit-accept, never derived from
    /// the prunable snapshot table.
    pub last_seq: i64,
    pub created_at: DateTime<Utc>,
}

/// One row of `deck_snapshots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckSnapshotRow {
    pub card_id: String,
    pub seq: i64,
    /// The card's own JSON text — opaque to the store.
    pub payload: String,
    pub fetched_at: DateTime<Utc>,
    /// Present when the tick failed; `payload` may be empty then.
    pub error: Option<String>,
}

/// One entry of a full-layout write (`PUT /v1/deck/layout`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckLayoutEntry {
    pub id: String,
    pub position: i64,
    pub size: DeckSize,
}

/// Deck persistence. The store is a dumb writer — bundle validation, the
/// dry-run gate, and quarantine policy live in `baybo-deck`.
#[async_trait]
pub trait DeckCardStore: Send + Sync {
    /// Insert a new card row verbatim.
    async fn create(&self, row: &DeckCardRow) -> Result<()>;

    /// Fetch one card by id, deleted or not.
    async fn get(&self, id: &str) -> Result<Option<DeckCardRow>>;

    /// Every non-deleted card, ordered by `position` ascending.
    async fn list_live(&self) -> Result<Vec<DeckCardRow>>;

    /// Recycle bin: deleted cards, most recently deleted first.
    async fn list_deleted(&self) -> Result<Vec<DeckCardRow>>;

    /// Count of non-deleted cards (the install cap check).
    async fn count_live(&self) -> Result<u64>;

    /// Full-layout write: every entry's `position` + `size` applied in one
    /// transaction so a partial reorder never lands. Unknown ids are
    /// ignored (the client may race a delete).
    async fn set_layout(&self, entries: &[DeckLayoutEntry]) -> Result<()>;

    /// Flip `enabled`. Returns `Ok(false)` if no row matched.
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool>;

    /// Stamp or clear `quarantined_at`. Returns `Ok(false)` if no row matched.
    async fn set_quarantined(&self, id: &str, at: Option<DateTime<Utc>>) -> Result<bool>;

    /// Update title + spec hash after a bundle update. Returns `Ok(false)`
    /// if no row matched.
    async fn set_installed(&self, id: &str, title: &str, spec_hash: &str) -> Result<bool>;

    /// Stamp (`Some` = soft delete) or clear (`None` = restore)
    /// `deleted_at`. Returns `Ok(false)` if no row matched.
    async fn set_deleted(&self, id: &str, at: Option<DateTime<Utc>>) -> Result<bool>;

    /// Hard delete: card row + its snapshots in one transaction. The
    /// caller removes the bundle directory. Returns `Ok(false)` if no row
    /// matched.
    async fn purge(&self, id: &str) -> Result<bool>;

    /// Accept an emitted snapshot: atomically bump `last_seq`, insert the
    /// snapshot under the new seq, and prune the card's older snapshots to
    /// the impl's retention. Returns the assigned seq.
    async fn record_snapshot(
        &self,
        card_id: &str,
        payload: &str,
        error: Option<&str>,
        fetched_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// Latest snapshot per non-deleted card (the deck's paint source).
    async fn latest_snapshots(&self) -> Result<Vec<DeckSnapshotRow>>;

    /// Latest snapshot for one card.
    async fn latest_snapshot(&self, card_id: &str) -> Result<Option<DeckSnapshotRow>>;
}
