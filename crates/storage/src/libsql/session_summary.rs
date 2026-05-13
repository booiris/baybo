//! libsql implementation of [`SessionSummaryStore`].

use async_trait::async_trait;
use aura_model::SessionId;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use crate::error::StorageError;
use crate::session_summary::{Result, SessionSummaryRow, SessionSummaryStore};

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
    let in_flight: i64 = row
        .get(8)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_summaries.in_flight: {e}")))?;
    let in_flight_owner: Option<String> = row.get(9).map_err(|e| {
        StorageError::Internal(anyhow::anyhow!("session_summaries.in_flight_owner: {e}"))
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
        in_flight: in_flight != 0,
        in_flight_owner,
    })
}

#[async_trait]
impl SessionSummaryStore for LibsqlSessionSummaryStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<SessionSummaryRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT session_id, cursor, pass_count, updated_at, \
                        cost_micros, model_id, span_id, error_count, in_flight, in_flight_owner \
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
        // error_count + in_flight reset to zero on success. Clearing
        // in_flight (and in_flight_owner) here is the primary terminal
        // handler — the gate sets it true before emitting
        // `SystemSpawnRequest`, and a landed pass is the canonical
        // "done" signal.
        conn.execute(
            "INSERT INTO session_summaries \
                 (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count, in_flight, in_flight_owner) \
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, 0, 0, NULL) \
             ON CONFLICT(session_id) DO UPDATE SET \
                 cursor          = excluded.cursor, \
                 pass_count      = session_summaries.pass_count + 1, \
                 updated_at      = excluded.updated_at, \
                 cost_micros     = session_summaries.cost_micros + excluded.cost_micros, \
                 model_id        = excluded.model_id, \
                 span_id         = excluded.span_id, \
                 error_count     = 0, \
                 in_flight       = 0, \
                 in_flight_owner = NULL",
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
        // Clearing `in_flight` (and `in_flight_owner`) here is the
        // secondary terminal handler: a recorded failure means the
        // pass has stopped trying, so the gate must be free to emit
        // the next one.
        conn.execute(
            "INSERT INTO session_summaries \
                 (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count, in_flight, in_flight_owner) \
             VALUES (?1, 0, 0, ?2, 0, ?3, ?4, 1, 0, NULL) \
             ON CONFLICT(session_id) DO UPDATE SET \
                 error_count     = session_summaries.error_count + 1, \
                 model_id        = excluded.model_id, \
                 span_id         = excluded.span_id, \
                 updated_at      = excluded.updated_at, \
                 in_flight       = 0, \
                 in_flight_owner = NULL",
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

    async fn set_in_flight(
        &self,
        session_id: &SessionId,
        in_flight: bool,
        owner: Option<&str>,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.pool.conn();
        // UPSERT: place a placeholder row (cursor=0, model="", span="")
        // when none exists so the gate can mark in_flight before the
        // first successful pass has ever recorded. On conflict, only
        // `in_flight`, `in_flight_owner`, and `updated_at` are touched
        // — cursor / pass_count / cost_micros / error_count are left
        // intact.
        conn.execute(
            "INSERT INTO session_summaries \
                 (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count, in_flight, in_flight_owner) \
             VALUES (?1, 0, 0, ?2, 0, '', '', 0, ?3, ?4) \
             ON CONFLICT(session_id) DO UPDATE SET \
                 in_flight       = excluded.in_flight, \
                 in_flight_owner = excluded.in_flight_owner, \
                 updated_at      = excluded.updated_at",
            libsql::params![
                session_id.as_str().to_string(),
                super::time::to_us(updated_at),
                if in_flight { 1_i64 } else { 0_i64 },
                owner.map(|s| s.to_string()),
            ],
        )
        .await
        .map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql set_in_flight session_summaries: {e}"))
        })?;
        Ok(())
    }

    async fn clear_in_flight_if_owned(
        &self,
        session_id: &SessionId,
        owner: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE session_summaries \
                 SET in_flight = 0, in_flight_owner = NULL, updated_at = ?2 \
                 WHERE session_id = ?1 AND in_flight_owner = ?3",
                libsql::params![
                    session_id.as_str().to_string(),
                    super::time::to_us(updated_at),
                    owner.to_string(),
                ],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!(
                    "libsql clear_in_flight_if_owned session_summaries: {e}"
                ))
            })?;
        Ok(affected > 0)
    }

    async fn clear_all_in_flight(&self) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE session_summaries SET in_flight = 0, in_flight_owner = NULL \
             WHERE in_flight = 1 OR in_flight_owner IS NOT NULL",
            (),
        )
        .await
        .map_err(|e| {
            StorageError::Internal(anyhow::anyhow!(
                "libsql clear_all_in_flight session_summaries: {e}"
            ))
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
    use crate::session::SessionStore;
    use aura_model::{ChannelType, Session, SessionState, TriggerSource, User};

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
            bound_soul_version: "soul-v1".into(),
            hidden: false,
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

    /// `set_in_flight(true)` on a session that has no metadata row
    /// yet should INSERT a placeholder row carrying `in_flight = 1`,
    /// `cursor = 0`, empty `model_id` / `span_id`, and zero counters.
    /// This is the trigger gate's "first ever pass" path.
    #[tokio::test]
    async fn set_in_flight_creates_placeholder_when_missing() {
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s-in-flight-1")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
        let id = SessionId::from("s-in-flight-1");

        store
            .set_in_flight(&id, true, Some("owner-token-1"), Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(row.in_flight);
        assert_eq!(row.in_flight_owner.as_deref(), Some("owner-token-1"));
        assert_eq!(row.cursor, 0);
        assert_eq!(row.pass_count, 0);
        assert_eq!(row.error_count, 0);
        assert_eq!(row.model_id, "");
        assert_eq!(row.span_id, "");
    }

    /// `set_in_flight(false)` on an existing row toggles only the flag
    /// (and `updated_at`) — never overwrites cursor, pass_count, cost,
    /// model_id, span_id, or error_count.
    #[tokio::test]
    async fn set_in_flight_preserves_existing_metadata() {
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s-in-flight-2")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
        let id = SessionId::from("s-in-flight-2");

        store
            .upsert_success(&id, 42, 12_345, "claude-x", "span-x", Utc::now())
            .await
            .unwrap();
        store
            .set_in_flight(&id, true, Some("owner-token-2"), Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(row.in_flight);
        assert_eq!(row.cursor, 42);
        assert_eq!(row.pass_count, 1);
        assert_eq!(row.cost_micros, 12_345);
        assert_eq!(row.model_id, "claude-x");
        assert_eq!(row.span_id, "span-x");

        store
            .set_in_flight(&id, false, None, Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(!row.in_flight);
        assert!(row.in_flight_owner.is_none());
        assert_eq!(row.cursor, 42);
        assert_eq!(row.pass_count, 1);
    }

    /// A successful pass on a session that was marked in-flight by the
    /// gate clears the flag — the gate's "is a pass running?" lookup
    /// will return false on the next iteration.
    #[tokio::test]
    async fn upsert_success_clears_in_flight() {
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s-clear-1")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
        let id = SessionId::from("s-clear-1");

        store
            .set_in_flight(&id, true, Some("owner-token-3"), Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(row.in_flight);
        assert_eq!(row.in_flight_owner.as_deref(), Some("owner-token-3"));

        store
            .upsert_success(&id, 5, 100, "m", "span", Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(!row.in_flight);
        assert!(
            row.in_flight_owner.is_none(),
            "upsert_success must NULL the owner on terminal landing"
        );
    }

    /// Compare-and-clear: only the owning pass can clear the mark, so
    /// a stale Pass A finishing after Pass B remarked the parent
    /// cannot wipe Pass B's owner.
    #[tokio::test]
    async fn clear_in_flight_if_owned_only_clears_when_owner_matches() {
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s-cas-1")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
        let id = SessionId::from("s-cas-1");

        // Pass A marks itself in-flight.
        store
            .set_in_flight(&id, true, Some("owner-A"), Utc::now())
            .await
            .unwrap();
        // Pass A finishes (record_summary_success) → mark cleared,
        // owner reset to NULL.
        store
            .upsert_success(&id, 5, 100, "m", "span", Utc::now())
            .await
            .unwrap();
        // Pass B picks up the parent and marks itself.
        store
            .set_in_flight(&id, true, Some("owner-B"), Utc::now())
            .await
            .unwrap();
        // Pass A's stale defensive cleanup with token A — must NOT
        // clear Pass B's mark.
        let cleared = store
            .clear_in_flight_if_owned(&id, "owner-A", Utc::now())
            .await
            .unwrap();
        assert!(!cleared, "stale owner token must not clear newer mark");

        let row = store.get(&id).await.unwrap().unwrap();
        assert!(row.in_flight);
        assert_eq!(row.in_flight_owner.as_deref(), Some("owner-B"));

        // Pass B's matching defensive cleanup with token B — clears.
        let cleared = store
            .clear_in_flight_if_owned(&id, "owner-B", Utc::now())
            .await
            .unwrap();
        assert!(cleared, "matching owner token must clear");
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(!row.in_flight);
        assert!(row.in_flight_owner.is_none());
    }

    /// Same invariant for the failure path: a recorded failure means
    /// the pass is no longer running, so the gate must be free to emit
    /// the next trigger.
    #[tokio::test]
    async fn bump_error_count_clears_in_flight() {
        let pool = fresh_pool().await;
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("s-clear-2")).await.unwrap();
        let store = LibsqlSessionSummaryStore::new(pool);
        let id = SessionId::from("s-clear-2");

        store
            .set_in_flight(&id, true, Some("owner-bump"), Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(row.in_flight);
        assert_eq!(row.in_flight_owner.as_deref(), Some("owner-bump"));

        store
            .bump_error_count(&id, "m", "span-err", Utc::now())
            .await
            .unwrap();
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(!row.in_flight);
        assert!(row.in_flight_owner.is_none());
        assert_eq!(row.error_count, 1);
    }
}
