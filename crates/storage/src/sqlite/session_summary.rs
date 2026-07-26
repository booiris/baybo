//! sqlite implementation of [`SessionSummaryStore`].

use async_trait::async_trait;
use baybo_model::SessionId;
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::session_summary::{Result, SessionSummaryRow, SessionSummaryStore};

pub struct SqliteSessionSummaryStore {
    pool: SqlitePool,
}

impl SqliteSessionSummaryStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Raw column tuple: (session_id, cursor, pass_count, updated_at µs,
/// cost_micros, model_id, span_id, error_count). Decoded into a
/// [`SessionSummaryRow`] outside the pool closure so the out-of-range
/// timestamp keeps its `StorageError::Storage` variant.
type RawSummaryRow = (String, i64, i64, i64, i64, String, String, i64);

fn row_from_raw(raw: RawSummaryRow) -> Result<SessionSummaryRow> {
    let (
        session_id,
        cursor,
        pass_count,
        updated_at_us,
        cost_micros,
        model_id,
        span_id,
        error_count,
    ) = raw;
    let updated_at = super::time::from_us(updated_at_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "session_summaries.updated_at out of range: {updated_at_us}"
        ))
    })?;
    Ok(SessionSummaryRow {
        session_id: SessionId::from(session_id),
        cursor,
        pass_count,
        updated_at,
        cost_micros,
        model_id,
        span_id,
        error_count,
    })
}

#[async_trait]
impl SessionSummaryStore for SqliteSessionSummaryStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<SessionSummaryRow>> {
        let sid = session_id.as_str().to_string();
        let raw = self
            .pool
            .interact("session_summaries.get", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT session_id, cursor, pass_count, updated_at, \
                                cost_micros, model_id, span_id, error_count \
                         FROM session_summaries WHERE session_id = ?1",
                        rusqlite::params![sid],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                                row.get(6)?,
                                row.get(7)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;
        match raw {
            Some(raw) => Ok(Some(row_from_raw(raw)?)),
            None => Ok(None),
        }
    }

    async fn upsert_success(
        &self,
        session_id: &SessionId,
        cursor: i64,
        cost_micros_delta: i64,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let sid = session_id.as_str().to_string();
        let model_id = model_id.to_string();
        let span_id = span_id.to_string();
        let updated_at_us = super::time::to_us(updated_at);
        self.pool
            .interact("session_summaries.upsert_success", move |conn| {
                // Single-statement upsert: INSERT … ON CONFLICT DO UPDATE so
                // pass_count and cost_micros increment atomically and
                // error_count resets to zero on success.
                //
                // `cursor` is MAX()'d, never overwritten: a pass pins
                // `up_to_ordinal` at trigger time but lands its row seconds to
                // minutes later, and a compaction in that window supersedes
                // every row it covered and `repoint_cursor`s onto the freshly
                // inserted continuation-summary row. A plain assignment would
                // drag the cursor back onto a superseded ordinal, which
                // `lookup_anchor_index_for_cursor` can't resolve — so
                // `tokens_since_anchor()` reads the whole transcript, the diff
                // gate is satisfied forever, and the fast path stays dead
                // until the next pass lands. Ordinals are append-only, so the
                // later pointer is always the live one.
                conn.execute(
                    "INSERT INTO session_summaries \
                         (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count) \
                     VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, 0) \
                     ON CONFLICT(session_id) DO UPDATE SET \
                         cursor          = MAX(session_summaries.cursor, excluded.cursor), \
                         pass_count      = session_summaries.pass_count + 1, \
                         updated_at      = excluded.updated_at, \
                         cost_micros     = session_summaries.cost_micros + excluded.cost_micros, \
                         model_id        = excluded.model_id, \
                         span_id         = excluded.span_id, \
                         error_count     = 0",
                    rusqlite::params![
                        sid,
                        cursor,
                        updated_at_us,
                        cost_micros_delta,
                        model_id,
                        span_id,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn repoint_cursor(
        &self,
        session_id: &SessionId,
        cursor: i64,
        updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        let updated_at_us = super::time::to_us(updated_at);
        let affected = self
            .pool
            .interact("session_summaries.repoint_cursor", move |conn| {
                // Plain UPDATE, never an insert: a cursor re-point is only
                // meaningful when a real pass already recorded coverage.
                Ok(conn.execute(
                    "UPDATE session_summaries SET cursor = ?2, updated_at = ?3 \
                     WHERE session_id = ?1",
                    rusqlite::params![sid, cursor, updated_at_us],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn bump_error_count(
        &self,
        session_id: &SessionId,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let sid = session_id.as_str().to_string();
        let model_id = model_id.to_string();
        let span_id = span_id.to_string();
        let updated_at_us = super::time::to_us(updated_at);
        self.pool
            .interact("session_summaries.bump_error_count", move |conn| {
                // Inserts a row with cursor=0 / pass_count=0 if absent so even
                // sessions that have never produced a successful summary can
                // surface failure telemetry. On conflict, increments error_count
                // and refreshes the model_id / span_id / updated_at trio.
                conn.execute(
                    "INSERT INTO session_summaries \
                         (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count) \
                     VALUES (?1, 0, 0, ?2, 0, ?3, ?4, 1) \
                     ON CONFLICT(session_id) DO UPDATE SET \
                         error_count     = session_summaries.error_count + 1, \
                         model_id        = excluded.model_id, \
                         span_id         = excluded.span_id, \
                         updated_at      = excluded.updated_at",
                    rusqlite::params![sid, updated_at_us, model_id, span_id],
                )?;
                Ok(())
            })
            .await
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        let affected = self
            .pool
            .interact("session_summaries.delete", move |conn| {
                Ok(conn.execute(
                    "DELETE FROM session_summaries WHERE session_id = ?1",
                    rusqlite::params![sid],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn list_session_ids(&self) -> Result<Vec<SessionId>> {
        self.pool
            .interact("session_summaries.list_session_ids", move |conn| {
                let mut stmt =
                    conn.prepare("SELECT session_id FROM session_summaries ORDER BY session_id")?;
                let ids = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(ids.into_iter().map(SessionId::from).collect())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::session::SqliteSessionStore;
    use baybo_model::{ChannelType, Session, SessionState, TriggerSource, User};
    use baybo_store::SessionStore;

    fn make_session(id: &str) -> Session {
        let user = User {
            id: format!("u-{id}"),
            name: None,
            channel: ChannelType::tui(),
        };
        let now = Utc::now();
        Session {
            id: SessionId::from(id),
            user,
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: SessionId::from(id),
            trigger: TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    /// Hands back the `TempDir` too: dropping it deletes the database file
    /// and its `-wal`/`-shm` siblings out from under the live connections.
    async fn fresh_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        (tmpdir, pool)
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session("s1")).await.unwrap();
        let store = SqliteSessionSummaryStore::new(pool);

        let id = SessionId::from("s1");
        let now = Utc::now();
        store
            .upsert_success(&id, 42, 12_345, "claude-opus-4-7", "span-abc", now)
            .await
            .unwrap();

        let row = store.get(&id).await.unwrap().unwrap();
        assert_eq!(row.session_id, id);
        assert_eq!(row.cursor, 42);
        assert_eq!(row.pass_count, 1);
        assert_eq!(row.cost_micros, 12_345);
        assert_eq!(row.model_id, "claude-opus-4-7");
        assert_eq!(row.span_id, "span-abc");
        assert_eq!(row.error_count, 0);
    }

    #[tokio::test]
    async fn upsert_increments_pass_count_and_accumulates_cost() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session("s2")).await.unwrap();
        let store = SqliteSessionSummaryStore::new(pool);
        let id = SessionId::from("s2");

        store
            .upsert_success(&id, 10, 1_000, "m", "span-1", Utc::now())
            .await
            .unwrap();
        store
            .upsert_success(&id, 20, 2_000, "m", "span-2", Utc::now())
            .await
            .unwrap();
        store
            .upsert_success(&id, 30, 500, "m", "span-3", Utc::now())
            .await
            .unwrap();

        let row = store.get(&id).await.unwrap().unwrap();
        assert_eq!(row.cursor, 30);
        assert_eq!(row.pass_count, 3);
        assert_eq!(row.cost_micros, 3_500);
        assert_eq!(row.span_id, "span-3");
    }

    #[tokio::test]
    async fn repoint_cursor_moves_cursor_without_touching_telemetry() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session("s-repoint")).await.unwrap();
        let store = SqliteSessionSummaryStore::new(pool);
        let id = SessionId::from("s-repoint");

        store
            .upsert_success(&id, 10, 1_000, "m", "span-1", Utc::now())
            .await
            .unwrap();

        let moved = store.repoint_cursor(&id, 99, Utc::now()).await.unwrap();
        assert!(moved);
        let row = store.get(&id).await.unwrap().unwrap();
        assert_eq!(row.cursor, 99);
        assert_eq!(row.pass_count, 1, "repoint must not bump pass_count");
        assert_eq!(row.cost_micros, 1_000, "repoint must not touch cost");
        assert_eq!(row.error_count, 0);
        assert_eq!(row.span_id, "span-1", "repoint must not touch span_id");
    }

    #[tokio::test]
    async fn repoint_cursor_never_inserts_a_missing_row() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session("s-absent")).await.unwrap();
        let store = SqliteSessionSummaryStore::new(pool);
        let id = SessionId::from("s-absent");

        let moved = store.repoint_cursor(&id, 5, Utc::now()).await.unwrap();
        assert!(!moved, "no row -> nothing to re-point");
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bump_error_count_creates_row_when_missing_and_increments_on_conflict() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session("s3")).await.unwrap();
        let store = SqliteSessionSummaryStore::new(pool);
        let id = SessionId::from("s3");

        // No prior row → bump creates one with error_count = 1.
        store
            .bump_error_count(&id, "m", "span-err-1", Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert_eq!(row.error_count, 1);
        assert_eq!(row.cursor, 0);
        assert_eq!(row.pass_count, 0);

        // Second bump increments.
        store
            .bump_error_count(&id, "m", "span-err-2", Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert_eq!(row.error_count, 2);
        assert_eq!(row.span_id, "span-err-2");
    }

    #[tokio::test]
    async fn upsert_after_error_resets_error_count() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session("s4")).await.unwrap();
        let store = SqliteSessionSummaryStore::new(pool);
        let id = SessionId::from("s4");

        store
            .bump_error_count(&id, "m", "span-1", Utc::now())
            .await
            .unwrap();
        store
            .bump_error_count(&id, "m", "span-2", Utc::now())
            .await
            .unwrap();

        store
            .upsert_success(&id, 5, 100, "m", "span-ok", Utc::now())
            .await
            .unwrap();

        let row = store.get(&id).await.unwrap().unwrap();
        assert_eq!(row.error_count, 0);
        assert_eq!(row.pass_count, 1);
        assert_eq!(row.cost_micros, 100);
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session("s5")).await.unwrap();
        let store = SqliteSessionSummaryStore::new(pool);
        let id = SessionId::from("s5");

        store
            .upsert_success(&id, 1, 1, "m", "span", Utc::now())
            .await
            .unwrap();
        assert!(store.delete(&id).await.unwrap());
        assert!(!store.delete(&id).await.unwrap());
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_session_ids_returns_sorted_set() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        for id in ["b", "a", "c"] {
            sessions.save(&make_session(id)).await.unwrap();
        }
        let store = SqliteSessionSummaryStore::new(pool);
        for id in ["b", "a", "c"] {
            store
                .upsert_success(&SessionId::from(id), 1, 1, "m", "span", Utc::now())
                .await
                .unwrap();
        }

        let ids = store.list_session_ids().await.unwrap();
        assert_eq!(
            ids,
            vec![
                SessionId::from("a"),
                SessionId::from("b"),
                SessionId::from("c"),
            ]
        );
    }

    #[tokio::test]
    async fn cascade_delete_when_parent_session_removed() {
        let (_tmpdir, pool) = fresh_pool().await;
        let sessions = SqliteSessionStore::new(pool.clone());
        let s = make_session("s-cascade");
        sessions.save(&s).await.unwrap();

        let store = SqliteSessionSummaryStore::new(pool);
        store
            .upsert_success(&s.id, 1, 1, "m", "span", Utc::now())
            .await
            .unwrap();
        assert!(store.get(&s.id).await.unwrap().is_some());

        sessions.delete(&s.id).await.unwrap();
        assert!(
            store.get(&s.id).await.unwrap().is_none(),
            "summary row must cascade-delete with parent session"
        );
    }
}
