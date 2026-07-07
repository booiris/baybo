//! libsql implementation of [`SessionSummaryStore`].

use async_trait::async_trait;
use baybo_model::SessionId;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use baybo_store::StorageError;
use baybo_store::session_summary::{Result, SessionSummaryRow, SessionSummaryStore};

pub struct LibsqlSessionSummaryStore {
    pool: LibsqlPool,
}

impl LibsqlSessionSummaryStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

fn row_from_libsql(row: &libsql::Row) -> Result<SessionSummaryRow> {
    let session_id: String = row.get(0).map_err(|e| {
        StorageError::Internal(anyhow::anyhow!("session_summaries.session_id: {e}"))
    })?;
    let cursor: i64 = row
        .get(1)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_summaries.cursor: {e}")))?;
    let pass_count: i64 = row.get(2).map_err(|e| {
        StorageError::Internal(anyhow::anyhow!("session_summaries.pass_count: {e}"))
    })?;
    let updated_at_us: i64 = row.get(3).map_err(|e| {
        StorageError::Internal(anyhow::anyhow!("session_summaries.updated_at: {e}"))
    })?;
    let cost_micros: i64 = row.get(4).map_err(|e| {
        StorageError::Internal(anyhow::anyhow!("session_summaries.cost_micros: {e}"))
    })?;
    let model_id: String = row
        .get(5)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_summaries.model_id: {e}")))?;
    let span_id: String = row
        .get(6)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_summaries.span_id: {e}")))?;
    let error_count: i64 = row.get(7).map_err(|e| {
        StorageError::Internal(anyhow::anyhow!("session_summaries.error_count: {e}"))
    })?;
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
impl SessionSummaryStore for LibsqlSessionSummaryStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<SessionSummaryRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT session_id, cursor, pass_count, updated_at, \
                        cost_micros, model_id, span_id, error_count \
                 FROM session_summaries WHERE session_id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query session_summaries: {e}"))
            })?;
        let row = rows.next().await.map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql session_summaries row: {e}"))
        })?;
        match row {
            Some(r) => Ok(Some(row_from_libsql(&r)?)),
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
        let conn = self.pool.conn();
        // Single-statement upsert: INSERT … ON CONFLICT DO UPDATE so
        // pass_count and cost_micros increment atomically and
        // error_count resets to zero on success.
        conn.execute(
            "INSERT INTO session_summaries \
                 (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count) \
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, 0) \
             ON CONFLICT(session_id) DO UPDATE SET \
                 cursor          = excluded.cursor, \
                 pass_count      = session_summaries.pass_count + 1, \
                 updated_at      = excluded.updated_at, \
                 cost_micros     = session_summaries.cost_micros + excluded.cost_micros, \
                 model_id        = excluded.model_id, \
                 span_id         = excluded.span_id, \
                 error_count     = 0",
            libsql::params![
                session_id.as_str().to_string(),
                cursor,
                super::time::to_us(updated_at),
                cost_micros_delta,
                model_id.to_string(),
                span_id.to_string(),
            ],
        )
        .await
        .map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql upsert session_summaries: {e}"))
        })?;
        Ok(())
    }

    async fn bump_error_count(
        &self,
        session_id: &SessionId,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.pool.conn();
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
            libsql::params![
                session_id.as_str().to_string(),
                super::time::to_us(updated_at),
                model_id.to_string(),
                span_id.to_string(),
            ],
        )
        .await
        .map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql bump session_summaries error: {e}"))
        })?;
        Ok(())
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "DELETE FROM session_summaries WHERE session_id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql delete session_summaries: {e}"))
            })?;
        Ok(affected > 0)
    }

    async fn list_session_ids(&self) -> Result<Vec<SessionId>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT session_id FROM session_summaries ORDER BY session_id",
                libsql::params![],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql list session_summaries: {e}"))
            })?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql session_summaries row: {e}"))
        })? {
            let id: String = row.get(0).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("session_summaries.session_id: {e}"))
            })?;
            out.push(SessionId::from(id));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libsql::session::LibsqlSessionStore;
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
            folder_id: None,
            title: None,
        }
    }

    async fn fresh_pool() -> LibsqlPool {
        LibsqlPool::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s1")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);

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
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s2")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
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
    async fn bump_error_count_creates_row_when_missing_and_increments_on_conflict() {
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s3")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
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
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s4")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
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
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s5")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
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
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        for id in ["b", "a", "c"] {
            sessions.save(&make_session(id)).await.unwrap();
        }
        let store = LibsqlSessionSummaryStore::new(pool);
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
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        let s = make_session("s-cascade");
        sessions.save(&s).await.unwrap();

        let store = LibsqlSessionSummaryStore::new(pool);
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
