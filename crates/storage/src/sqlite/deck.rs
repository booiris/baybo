//! sqlite implementation of [`DeckCardStore`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::deck::{
    DeckCardRow, DeckCardStore, DeckLayoutEntry, DeckSize, DeckSnapshotRow, Result,
};

/// Snapshots retained per card after a `record_snapshot` prune. Snapshots
/// are ephemeral render state — the newest paints the card, a couple of
/// predecessors survive for debugging a bad tick. Retention is
/// prune-on-insert here (a plain `DELETE`); no background sweeper is
/// involved.
const SNAPSHOT_KEEP: i64 = 3;

pub struct SqliteDeckCardStore {
    pool: SqlitePool,
}

impl SqliteDeckCardStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Raw column tuple: (id, title, position, size, sizes, maximize, enabled,
/// quarantined_at µs, deleted_at µs, spec_hash, last_seq, created_at µs).
type RawCard = (
    String,
    String,
    i64,
    String,
    String,
    bool,
    bool,
    Option<i64>,
    Option<i64>,
    String,
    i64,
    i64,
);

const CARD_COLUMNS: &str = "id, title, position, size, sizes, maximize, enabled, \
     quarantined_at, deleted_at, spec_hash, last_seq, created_at";

fn card_from_raw(raw: RawCard) -> Result<DeckCardRow> {
    let (
        id,
        title,
        position,
        size,
        sizes,
        maximize,
        enabled,
        quarantined_us,
        deleted_us,
        spec_hash,
        last_seq,
        created_us,
    ) = raw;
    let size = DeckSize::parse(&size)
        .ok_or_else(|| StorageError::Storage(format!("deck_cards.size unknown: {size}")))?;
    // A blank column (legacy row) means the card declared no set — it is a
    // single-size, never-adapted card, so [size] is the honest fallback.
    let sizes = DeckSize::parse_list(&sizes).unwrap_or_else(|| vec![size]);
    let ts = |us: i64, col: &str| {
        super::time::from_us(us)
            .ok_or_else(|| StorageError::Storage(format!("deck_cards.{col} out of range: {us}")))
    };
    Ok(DeckCardRow {
        id,
        title,
        position,
        size,
        sizes,
        maximize,
        enabled,
        quarantined_at: quarantined_us
            .map(|us| ts(us, "quarantined_at"))
            .transpose()?,
        deleted_at: deleted_us.map(|us| ts(us, "deleted_at")).transpose()?,
        spec_hash,
        last_seq,
        created_at: ts(created_us, "created_at")?,
    })
}

fn read_raw_card(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawCard> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

/// Raw column tuple: (card_id, seq, payload, fetched_at µs, error).
type RawSnapshot = (String, i64, String, i64, Option<String>);

fn snapshot_from_raw(raw: RawSnapshot) -> Result<DeckSnapshotRow> {
    let (card_id, seq, payload, fetched_us, error) = raw;
    let fetched_at = super::time::from_us(fetched_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "deck_snapshots.fetched_at out of range: {fetched_us}"
        ))
    })?;
    Ok(DeckSnapshotRow {
        card_id,
        seq,
        payload,
        fetched_at,
        error,
    })
}

#[async_trait]
impl DeckCardStore for SqliteDeckCardStore {
    async fn create(&self, row: &DeckCardRow) -> Result<()> {
        let r = row.clone();
        let quarantined = r.quarantined_at.map(super::time::to_us);
        let deleted = r.deleted_at.map(super::time::to_us);
        let created = super::time::to_us(r.created_at);
        let sizes = DeckSize::join(&r.sizes);
        self.pool
            .interact("deck_cards.create", move |conn| {
                conn.execute(
                    "INSERT INTO deck_cards (id, title, position, size, sizes, maximize, \
                     enabled, quarantined_at, deleted_at, spec_hash, last_seq, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        r.id,
                        r.title,
                        r.position,
                        r.size.as_str(),
                        sizes,
                        r.maximize,
                        r.enabled,
                        quarantined,
                        deleted,
                        r.spec_hash,
                        r.last_seq,
                        created
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn get(&self, id: &str) -> Result<Option<DeckCardRow>> {
        let id = id.to_string();
        let raw = self
            .pool
            .interact("deck_cards.get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {CARD_COLUMNS} FROM deck_cards WHERE id = ?1"),
                        rusqlite::params![id],
                        read_raw_card,
                    )
                    .optional()?)
            })
            .await?;
        raw.map(card_from_raw).transpose()
    }

    async fn list_live(&self) -> Result<Vec<DeckCardRow>> {
        let raws = self
            .pool
            .interact("deck_cards.list_live", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {CARD_COLUMNS} FROM deck_cards \
                     WHERE deleted_at IS NULL ORDER BY position, id"
                ))?;
                let raws = stmt
                    .query_map([], read_raw_card)?
                    .collect::<rusqlite::Result<Vec<RawCard>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(card_from_raw).collect()
    }

    async fn list_deleted(&self) -> Result<Vec<DeckCardRow>> {
        let raws = self
            .pool
            .interact("deck_cards.list_deleted", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {CARD_COLUMNS} FROM deck_cards \
                     WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC, id"
                ))?;
                let raws = stmt
                    .query_map([], read_raw_card)?
                    .collect::<rusqlite::Result<Vec<RawCard>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(card_from_raw).collect()
    }

    async fn count_live(&self) -> Result<u64> {
        let count: i64 = self
            .pool
            .interact("deck_cards.count_live", move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM deck_cards WHERE deleted_at IS NULL",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await?;
        Ok(count.max(0) as u64)
    }

    async fn set_layout(&self, entries: &[DeckLayoutEntry]) -> Result<()> {
        let entries: Vec<(String, i64, &'static str)> = entries
            .iter()
            .map(|e| (e.id.clone(), e.position, e.size.as_str()))
            .collect();
        self.pool
            .interact("deck_cards.set_layout", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                for (id, position, size) in &entries {
                    tx.execute(
                        "UPDATE deck_cards SET position = ?2, size = ?3 \
                         WHERE id = ?1 AND deleted_at IS NULL",
                        rusqlite::params![id, position, size],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let id = id.to_string();
        let affected = self
            .pool
            .interact("deck_cards.set_enabled", move |conn| {
                Ok(conn.execute(
                    "UPDATE deck_cards SET enabled = ?2 WHERE id = ?1",
                    rusqlite::params![id, enabled],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_quarantined(&self, id: &str, at: Option<DateTime<Utc>>) -> Result<bool> {
        let id = id.to_string();
        let at = at.map(super::time::to_us);
        let affected = self
            .pool
            .interact("deck_cards.set_quarantined", move |conn| {
                Ok(conn.execute(
                    "UPDATE deck_cards SET quarantined_at = ?2 WHERE id = ?1",
                    rusqlite::params![id, at],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_installed(
        &self,
        id: &str,
        title: &str,
        spec_hash: &str,
        size: Option<DeckSize>,
        sizes: &[DeckSize],
        maximize: bool,
    ) -> Result<bool> {
        let id = id.to_string();
        let title = title.to_string();
        let spec_hash = spec_hash.to_string();
        // NULL leaves the existing size in place (COALESCE); a value re-clamps.
        let size = size.map(|s| s.as_str());
        let sizes = DeckSize::join(sizes);
        let affected = self
            .pool
            .interact("deck_cards.set_installed", move |conn| {
                Ok(conn.execute(
                    "UPDATE deck_cards SET title = ?2, spec_hash = ?3, \
                     size = COALESCE(?4, size), sizes = ?5, maximize = ?6 WHERE id = ?1",
                    rusqlite::params![id, title, spec_hash, size, sizes, maximize],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_deleted(&self, id: &str, at: Option<DateTime<Utc>>) -> Result<bool> {
        let id = id.to_string();
        let at = at.map(super::time::to_us);
        let affected = self
            .pool
            .interact("deck_cards.set_deleted", move |conn| {
                Ok(conn.execute(
                    "UPDATE deck_cards SET deleted_at = ?2 WHERE id = ?1",
                    rusqlite::params![id, at],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn purge(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.pool
            .interact("deck_cards.purge", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "DELETE FROM deck_snapshots WHERE card_id = ?1",
                    rusqlite::params![id],
                )?;
                let affected = tx.execute(
                    "DELETE FROM deck_cards WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                tx.commit()?;
                Ok(affected > 0)
            })
            .await
    }

    async fn record_snapshot(
        &self,
        card_id: &str,
        payload: &str,
        error: Option<&str>,
        fetched_at: DateTime<Utc>,
    ) -> Result<i64> {
        let card_id = card_id.to_string();
        let payload = payload.to_string();
        let error = error.map(|e| e.to_string());
        let fetched = super::time::to_us(fetched_at);
        self.pool
            .interact("deck_cards.record_snapshot", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let bumped = tx.execute(
                    "UPDATE deck_cards SET last_seq = last_seq + 1 \
                     WHERE id = ?1 AND deleted_at IS NULL",
                    rusqlite::params![card_id],
                )?;
                if bumped == 0 {
                    drop(tx);
                    return Err(anyhow::anyhow!("deck card not found or deleted: {card_id}"));
                }
                let seq: i64 = tx.query_row(
                    "SELECT last_seq FROM deck_cards WHERE id = ?1",
                    rusqlite::params![card_id],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "INSERT INTO deck_snapshots (card_id, seq, payload, fetched_at, error) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![card_id, seq, payload, fetched, error],
                )?;
                tx.execute(
                    "DELETE FROM deck_snapshots WHERE card_id = ?1 AND seq <= ?2",
                    rusqlite::params![card_id, seq - SNAPSHOT_KEEP],
                )?;
                tx.commit()?;
                Ok(seq)
            })
            .await
    }

    async fn latest_snapshots(&self) -> Result<Vec<DeckSnapshotRow>> {
        let raws = self
            .pool
            .interact("deck_cards.latest_snapshots", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT s.card_id, s.seq, s.payload, s.fetched_at, s.error \
                     FROM deck_snapshots s \
                     JOIN deck_cards c ON c.id = s.card_id AND c.deleted_at IS NULL \
                     WHERE s.seq = (SELECT MAX(seq) FROM deck_snapshots \
                                    WHERE card_id = s.card_id) \
                     ORDER BY s.card_id",
                )?;
                let raws = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<RawSnapshot>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(snapshot_from_raw).collect()
    }

    async fn latest_snapshot(&self, card_id: &str) -> Result<Option<DeckSnapshotRow>> {
        let card_id = card_id.to_string();
        let raw = self
            .pool
            .interact("deck_cards.latest_snapshot", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT card_id, seq, payload, fetched_at, error \
                         FROM deck_snapshots WHERE card_id = ?1 \
                         ORDER BY seq DESC LIMIT 1",
                        rusqlite::params![card_id],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;
        raw.map(snapshot_from_raw).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, pos: i64) -> DeckCardRow {
        DeckCardRow {
            id: id.to_string(),
            title: format!("card {id}"),
            position: pos,
            size: DeckSize::Wide,
            sizes: vec![DeckSize::Wide, DeckSize::Large],
            maximize: false,
            enabled: true,
            quarantined_at: None,
            deleted_at: None,
            spec_hash: "hash0".into(),
            last_seq: 0,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn create_get_list_roundtrip() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteDeckCardStore::new(pool);
        store.create(&card("a", 1)).await.unwrap();
        store.create(&card("b", 0)).await.unwrap();

        let live = store.list_live().await.unwrap();
        assert_eq!(
            live.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"],
            "ordered by position"
        );
        assert_eq!(store.count_live().await.unwrap(), 2);

        let a = store.get("a").await.unwrap().unwrap();
        assert_eq!(a.size, DeckSize::Wide);
        assert_eq!(a.sizes, vec![DeckSize::Wide, DeckSize::Large]);
        assert!(!a.maximize);
        assert!(a.enabled);
        assert!(store.get("ghost").await.unwrap().is_none());
    }

    /// A row written before the `sizes` column existed carries the ALTER
    /// default (empty string); the reader must fall back to `[size]` — a
    /// single-size, never-adapted legacy card — not error or empty-list.
    #[tokio::test]
    async fn legacy_row_without_sizes_falls_back_to_single_size() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteDeckCardStore::new(pool.clone());
        // Insert bypassing `create` to leave `sizes` at the column default.
        let now_us = super::super::time::to_us(chrono::Utc::now());
        pool.interact("test.legacy_insert", move |conn| {
            conn.execute(
                "INSERT INTO deck_cards (id, title, position, size, spec_hash, created_at) \
                 VALUES ('legacy', 't', 0, 'large', 'h', ?1)",
                rusqlite::params![now_us],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let row = store.get("legacy").await.unwrap().unwrap();
        assert_eq!(row.size, DeckSize::Large);
        assert_eq!(row.sizes, vec![DeckSize::Large], "empty column → [size]");
        assert!(!row.maximize, "maximize defaults false");
    }

    #[tokio::test]
    async fn layout_enable_quarantine_installed() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteDeckCardStore::new(pool);
        store.create(&card("a", 0)).await.unwrap();
        store.create(&card("b", 1)).await.unwrap();

        store
            .set_layout(&[
                DeckLayoutEntry {
                    id: "a".into(),
                    position: 1,
                    size: DeckSize::Large,
                },
                DeckLayoutEntry {
                    id: "b".into(),
                    position: 0,
                    size: DeckSize::Small,
                },
                // Unknown id is ignored, not an error.
                DeckLayoutEntry {
                    id: "ghost".into(),
                    position: 9,
                    size: DeckSize::Small,
                },
            ])
            .await
            .unwrap();
        let live = store.list_live().await.unwrap();
        assert_eq!(live[0].id, "b");
        assert_eq!(live[0].size, DeckSize::Small);
        assert_eq!(live[1].size, DeckSize::Large);

        assert!(store.set_enabled("a", false).await.unwrap());
        assert!(!store.get("a").await.unwrap().unwrap().enabled);

        let now = chrono::Utc::now();
        assert!(store.set_quarantined("a", Some(now)).await.unwrap());
        assert!(
            store
                .get("a")
                .await
                .unwrap()
                .unwrap()
                .quarantined_at
                .is_some()
        );
        assert!(store.set_quarantined("a", None).await.unwrap());

        assert!(
            store
                .set_installed(
                    "a",
                    "renamed",
                    "hash1",
                    Some(DeckSize::Small),
                    &[DeckSize::Small],
                    true,
                )
                .await
                .unwrap()
        );
        let a = store.get("a").await.unwrap().unwrap();
        assert_eq!(a.title, "renamed");
        assert_eq!(a.spec_hash, "hash1");
        // A Some(size) re-clamps and refreshes the capability set.
        assert_eq!(a.size, DeckSize::Small);
        assert_eq!(a.sizes, vec![DeckSize::Small]);
        assert!(a.maximize);

        // A None size leaves the size column untouched (a concurrent layout
        // resize is not clobbered) while still refreshing title/hash/sizes.
        assert!(
            store
                .set_installed(
                    "a",
                    "again",
                    "hash2",
                    None,
                    &[DeckSize::Small, DeckSize::Wide],
                    false,
                )
                .await
                .unwrap()
        );
        let a = store.get("a").await.unwrap().unwrap();
        assert_eq!(a.size, DeckSize::Small, "size untouched by None");
        assert_eq!(a.sizes, vec![DeckSize::Small, DeckSize::Wide]);
        assert!(!a.maximize);

        assert!(!store.set_enabled("ghost", true).await.unwrap());
    }

    #[tokio::test]
    async fn soft_delete_restore_purge() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteDeckCardStore::new(pool);
        store.create(&card("a", 0)).await.unwrap();

        let now = chrono::Utc::now();
        assert!(store.set_deleted("a", Some(now)).await.unwrap());
        assert!(store.list_live().await.unwrap().is_empty());
        assert_eq!(store.count_live().await.unwrap(), 0);
        assert_eq!(store.list_deleted().await.unwrap().len(), 1);
        // Deleted rows still resolve by id (restore needs them).
        assert!(store.get("a").await.unwrap().is_some());

        assert!(store.set_deleted("a", None).await.unwrap());
        assert_eq!(store.list_live().await.unwrap().len(), 1);

        assert!(store.purge("a").await.unwrap());
        assert!(store.get("a").await.unwrap().is_none());
        assert!(!store.purge("a").await.unwrap());
    }

    #[tokio::test]
    async fn record_snapshot_bumps_seq_and_prunes() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteDeckCardStore::new(pool);
        store.create(&card("a", 0)).await.unwrap();

        let now = chrono::Utc::now();
        for i in 1..=5 {
            let seq = store
                .record_snapshot("a", &format!("payload{i}"), None, now)
                .await
                .unwrap();
            assert_eq!(seq, i);
        }
        // last_seq lives on the card row, not the snapshot table.
        assert_eq!(store.get("a").await.unwrap().unwrap().last_seq, 5);

        // Pruned to SNAPSHOT_KEEP; the latest is seq 5.
        let latest = store.latest_snapshot("a").await.unwrap().unwrap();
        assert_eq!(latest.seq, 5);
        assert_eq!(latest.payload, "payload5");

        let all = store.latest_snapshots().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].seq, 5);

        // Error ticks are recorded states too, and also bump the counter.
        let seq = store
            .record_snapshot("a", "", Some("boom"), now)
            .await
            .unwrap();
        assert_eq!(seq, 6);
        assert_eq!(
            store
                .latest_snapshot("a")
                .await
                .unwrap()
                .unwrap()
                .error
                .as_deref(),
            Some("boom")
        );

        // Deleted cards refuse snapshots (an emit racing a delete).
        store.set_deleted("a", Some(now)).await.unwrap();
        assert!(store.record_snapshot("a", "x", None, now).await.is_err());
        // A deleted card's snapshot is also out of the paint source.
        assert!(store.latest_snapshots().await.unwrap().is_empty());
    }
}
