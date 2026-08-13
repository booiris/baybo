use async_trait::async_trait;
use rusqlite::OptionalExtension;
use rusqlite::types::Value;

use super::SqlitePool;
use baybo_model::{SessionId, TurnId};
use baybo_store::turn::Result;
use baybo_store::{SessionTurnBounds, SessionTurnStats, StorageError, TurnRow, TurnStore};

pub struct SqliteTurnStore {
    pool: SqlitePool,
}

impl SqliteTurnStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const SELECT_COLS: &str = "id, session_id, parent_turn_id, kind, status_kind, \
     created_at, started_at, ended_at, data";

/// `status_kind` values of a turn that has NOT settled — the SQL spelling of
/// [`TurnStatusKind::needs_recovery`]. Three queries filter on exactly this
/// set (recover at boot, list a session's live turns, find who is mid-turn
/// right now), and the same three literals in three places is how they drift
/// apart; `status_sql_matches_the_enum` pins this against the enum.
const NON_TERMINAL_STATUS_SQL: &str = "'pending', 'in_progress', 'stuck'";

/// The `turns` columns exactly as sqlite hands them over, before any fallible
/// decoding (turn-id parse, µs → `DateTime`). The row closure of `query_map`
/// can only surface `rusqlite` errors, so decoding happens after the collect.
type RawTurnRow = (
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    String,
);

fn raw_turn_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTurnRow> {
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
    ))
}

fn parse_turn_id(s: String) -> anyhow::Result<TurnId> {
    s.parse::<TurnId>()
        .map_err(|e| anyhow::anyhow!("parse turn id: {e}"))
}

fn turn_row_from(raw: RawTurnRow) -> anyhow::Result<TurnRow> {
    let (id, session_id, parent_turn_id, kind, status_kind, created_at, started_at, ended_at, data) =
        raw;
    Ok(TurnRow {
        id: parse_turn_id(id)?,
        session_id: SessionId::from(session_id),
        parent_turn_id: parent_turn_id.map(parse_turn_id).transpose()?,
        kind,
        status_kind,
        created_at: super::time::from_us(created_at)
            .ok_or_else(|| anyhow::anyhow!("created_at: out of range"))?,
        started_at: started_at.and_then(super::time::from_us),
        ended_at: ended_at.and_then(super::time::from_us),
        data,
    })
}

#[async_trait]
impl TurnStore for SqliteTurnStore {
    async fn create(&self, turn: &TurnRow) -> Result<()> {
        let id = turn.id.to_string();
        let session_id = turn.session_id.as_str().to_string();
        let parent_turn_id = turn.parent_turn_id.map(|p| p.to_string());
        let kind = turn.kind.clone();
        let status_kind = turn.status_kind.clone();
        let created_at = super::time::to_us(turn.created_at);
        let started_at = turn.started_at.map(super::time::to_us);
        let ended_at = turn.ended_at.map(super::time::to_us);
        let data = turn.data.clone();
        self.pool
            .interact("turns.create", move |conn| {
                conn.execute(
                    "INSERT INTO turns \
                     (id, session_id, parent_turn_id, kind, status_kind, \
                      created_at, started_at, ended_at, data) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        id,
                        session_id,
                        parent_turn_id,
                        kind,
                        status_kind,
                        created_at,
                        started_at,
                        ended_at,
                        data,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn get(&self, turn_id: &TurnId) -> Result<Option<TurnRow>> {
        let id = turn_id.to_string();
        self.pool
            .interact("turns.get", move |conn| {
                let raw = conn
                    .query_row(
                        &format!("SELECT {SELECT_COLS} FROM turns WHERE id = ?1"),
                        rusqlite::params![id],
                        raw_turn_row,
                    )
                    .optional()?;
                raw.map(turn_row_from).transpose()
            })
            .await
    }

    async fn save(&self, turn: &TurnRow) -> Result<()> {
        let status_kind = turn.status_kind.clone();
        let started_at = turn.started_at.map(super::time::to_us);
        let ended_at = turn.ended_at.map(super::time::to_us);
        let data = turn.data.clone();
        let id = turn.id.to_string();
        let rows_affected = self
            .pool
            .interact("turns.save", move |conn| {
                Ok(conn.execute(
                    "UPDATE turns SET status_kind = ?1, started_at = ?2, ended_at = ?3, data = ?4 \
                     WHERE id = ?5",
                    rusqlite::params![status_kind, started_at, ended_at, data, id],
                )?)
            })
            .await?;
        if rows_affected == 0 {
            return Err(StorageError::NotFound(turn.id.to_string()));
        }
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<TurnRow>> {
        self.collect(
            "turns.list_by_session",
            format!("SELECT {SELECT_COLS} FROM turns WHERE session_id = ?1 ORDER BY created_at"),
            vec![Value::from(session_id.as_str().to_string())],
        )
        .await
    }

    async fn list_active_by_session(&self, session_id: &SessionId) -> Result<Vec<TurnRow>> {
        self.collect(
            "turns.list_active_by_session",
            format!(
                "SELECT {SELECT_COLS} FROM turns \
                 WHERE session_id = ?1 AND status_kind IN ({NON_TERMINAL_STATUS_SQL}) \
                 ORDER BY created_at"
            ),
            vec![Value::from(session_id.as_str().to_string())],
        )
        .await
    }

    async fn sessions_with_live_turns(&self) -> Result<Vec<SessionId>> {
        self.pool
            .interact("turns.sessions_with_live_turns", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT DISTINCT session_id FROM turns \
                     WHERE status_kind IN ({NON_TERMINAL_STATUS_SQL})"
                ))?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .map(|rows| rows.into_iter().map(SessionId::from).collect())
    }

    async fn list_by_status_kind(&self, status_kind: &str) -> Result<Vec<TurnRow>> {
        self.collect(
            "turns.list_by_status_kind",
            format!("SELECT {SELECT_COLS} FROM turns WHERE status_kind = ?1 ORDER BY created_at"),
            vec![Value::from(status_kind.to_string())],
        )
        .await
    }

    async fn list_recoverable(&self) -> Result<Vec<TurnRow>> {
        self.collect(
            "turns.list_recoverable",
            format!(
                "SELECT {SELECT_COLS} FROM turns \
                 WHERE status_kind IN ({NON_TERMINAL_STATUS_SQL}) ORDER BY created_at"
            ),
            vec![],
        )
        .await
    }

    async fn list_children(&self, parent_turn_id: &TurnId) -> Result<Vec<TurnRow>> {
        self.collect(
            "turns.list_children",
            format!(
                "SELECT {SELECT_COLS} FROM turns WHERE parent_turn_id = ?1 ORDER BY created_at"
            ),
            vec![Value::from(parent_turn_id.to_string())],
        )
        .await
    }

    async fn list_all(&self) -> Result<Vec<TurnRow>> {
        self.collect(
            "turns.list_all",
            format!("SELECT {SELECT_COLS} FROM turns"),
            vec![],
        )
        .await
    }

    async fn list_page(
        &self,
        status_kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<TurnRow>, usize)> {
        let status_kind = status_kind.map(str::to_string);
        let limit = limit as i64;
        let offset = offset as i64;
        self.pool
            .interact("turns.list_page", move |conn| {
                let (filter, params): (&str, Vec<Value>) = match &status_kind {
                    Some(k) => ("WHERE status_kind = ?1", vec![Value::from(k.clone())]),
                    None => ("", vec![]),
                };
                let total: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM turns {filter}"),
                    rusqlite::params_from_iter(params.iter()),
                    |row| row.get(0),
                )?;
                let mut page_params = params;
                page_params.push(Value::from(limit));
                page_params.push(Value::from(offset));
                let (limit_ref, offset_ref) = if status_kind.is_some() {
                    ("?2", "?3")
                } else {
                    ("?1", "?2")
                };
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLS} FROM turns {filter} \
                     ORDER BY created_at DESC, id DESC LIMIT {limit_ref} OFFSET {offset_ref}"
                ))?;
                let raws = stmt
                    .query_map(rusqlite::params_from_iter(page_params), raw_turn_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let rows = raws
                    .into_iter()
                    .map(turn_row_from)
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok((rows, total as usize))
            })
            .await
    }

    async fn session_turn_bounds(
        &self,
        session_ids: &[SessionId],
    ) -> Result<Vec<SessionTurnBounds>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<String> = session_ids.iter().map(|s| s.as_str().to_string()).collect();
        let raw: Vec<(String, i64, Option<i64>, i64, String)> = self
            .pool
            .interact("turns.session_turn_bounds", move |conn| {
                let placeholders = super::in_placeholders(keys.len());
                // `MIN(COALESCE(started_at, created_at))` because a queued turn
                // has no start yet but has already begun costing the user wall
                // clock. `MAX(ended_at)` is only meaningful once nothing is
                // open, which `open_turns` decides — SQLite's MAX skips NULLs,
                // so without that guard a still-running child would report the
                // end of its previous turn as its own.
                let mut stmt = conn.prepare(&format!(
                    "SELECT session_id, \
                            MIN(COALESCE(started_at, created_at)), \
                            MAX(ended_at), \
                            SUM(ended_at IS NULL), \
                            (SELECT t2.status_kind FROM turns t2 \
                              WHERE t2.session_id = turns.session_id \
                              ORDER BY t2.created_at DESC, t2.id DESC LIMIT 1) \
                     FROM turns WHERE session_id IN ({placeholders}) \
                     GROUP BY session_id"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(keys.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        Ok(raw
            .into_iter()
            .filter_map(
                |(session_id, first_us, last_end_us, open_turns, latest_status_kind)| {
                    let live = open_turns > 0;
                    Some(SessionTurnBounds {
                        session_id: SessionId::from(session_id),
                        // An out-of-range stamp drops the row rather than
                        // failing the whole listing: this feeds a display
                        // surface, and one corrupt child must not blank the
                        // parent's list.
                        first_started_at: super::time::from_us(first_us)?,
                        last_ended_at: if live {
                            None
                        } else {
                            last_end_us.and_then(super::time::from_us)
                        },
                        live,
                        latest_status_kind,
                    })
                },
            )
            .collect())
    }

    async fn count_by_status_kind(&self, status_kind: &str) -> Result<usize> {
        let status_kind = status_kind.to_string();
        self.pool
            .interact("turns.count_by_status_kind", move |conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM turns WHERE status_kind = ?1",
                    rusqlite::params![status_kind],
                    |row| row.get(0),
                )?;
                Ok(n as usize)
            })
            .await
    }

    async fn session_turn_stats(&self) -> Result<Vec<SessionTurnStats>> {
        self.pool
            .interact("turns.session_turn_stats", move |conn| {
                // The correlated subquery resolves the newest turn's
                // status_kind per group via idx_turns_session
                // (session_id, created_at) — an index seek per session,
                // no data-blob reads.
                let mut stmt = conn.prepare(
                    "SELECT session_id, COUNT(*), \
                            (SELECT j2.status_kind FROM turns j2 \
                              WHERE j2.session_id = turns.session_id \
                              ORDER BY j2.created_at DESC, j2.id DESC LIMIT 1) \
                     FROM turns GROUP BY session_id",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows
                    .into_iter()
                    .map(
                        |(session_id, turn_count, latest_status_kind)| SessionTurnStats {
                            session_id: SessionId::from(session_id),
                            turn_count: turn_count as usize,
                            latest_status_kind,
                        },
                    )
                    .collect())
            })
            .await
    }
}

impl SqliteTurnStore {
    async fn collect(
        &self,
        op: &'static str,
        sql: String,
        params: Vec<Value>,
    ) -> Result<Vec<TurnRow>> {
        self.pool
            .interact(op, move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let raws = stmt
                    .query_map(rusqlite::params_from_iter(params), raw_turn_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                raws.into_iter().map(turn_row_from).collect()
            })
            .await
    }
}

#[cfg(test)]
#[allow(unused_must_use)] // tests build state machines via direct calls; the TurnTransition audit record isn't the assertion target
mod tests {

    /// The SQL const and the enum are one fact in two places; the queries
    /// that filter on it decide what gets recovered at boot and what the
    /// dream pass leaves alone, so a drift here is silent and expensive.
    #[test]
    fn non_terminal_status_sql_matches_the_enum() {
        use baybo_turn::TurnStatusKind::*;
        let expected: Vec<String> = [Pending, InProgress, Stuck, Cancelled, Failed, Completed]
            .into_iter()
            .filter(|k| k.needs_recovery())
            .map(|k| format!("'{}'", k.as_snake_case()))
            .collect();
        assert_eq!(NON_TERMINAL_STATUS_SQL, expected.join(", "));
    }
    use super::*;
    use baybo_model::{ContentBlock, TriggerKind};
    use baybo_turn::{Turn, TurnInput, TurnStatus};

    fn test_turn() -> Turn {
        Turn::new(
            SessionId::from("sess-1"),
            TriggerKind::User,
            TurnInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        )
    }

    async fn create(store: &SqliteTurnStore, turn: &Turn) {
        store.create(&turn.to_row().unwrap()).await.unwrap();
    }

    /// The subagent list surface polls this while its sheet is open, for up to
    /// a hundred children at once — hence one grouped query rather than a
    /// per-child read. The load-bearing rule is that an OPEN turn erases the
    /// session's end: a `MAX(ended_at)` that skipped NULLs would report the
    /// previous turn's end as the current one's, and a running child's clock
    /// would appear to have stopped.
    #[tokio::test]
    async fn session_turn_bounds_groups_liveness_and_wall_clock() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);

        // A settled child: one completed turn.
        let mut done = test_turn();
        done.session_id = SessionId::from("child-done");
        done.start().unwrap();
        done.complete(baybo_turn::TurnOutput::Message {
            content: vec![ContentBlock::Text("ok".into())],
            ordinal: None,
        })
        .unwrap();
        create(&store, &done).await;

        // A running child: one completed turn AND one still open.
        let mut first = test_turn();
        first.session_id = SessionId::from("child-live");
        first.start().unwrap();
        first
            .complete(baybo_turn::TurnOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        create(&store, &first).await;
        let mut open = test_turn();
        open.session_id = SessionId::from("child-live");
        open.start().unwrap();
        create(&store, &open).await;

        // A child whose actor never opened a turn is simply absent.
        let bounds = store
            .session_turn_bounds(&[
                SessionId::from("child-done"),
                SessionId::from("child-live"),
                SessionId::from("child-unstarted"),
            ])
            .await
            .unwrap();
        assert_eq!(bounds.len(), 2, "no rows ⇒ no entry: {bounds:?}");

        let by_id: std::collections::HashMap<String, &SessionTurnBounds> = bounds
            .iter()
            .map(|b| (b.session_id.as_str().to_string(), b))
            .collect();

        let settled = by_id["child-done"];
        assert!(!settled.live);
        assert!(
            settled.last_ended_at.is_some(),
            "a settled child has an end"
        );
        assert_eq!(settled.latest_status_kind, "completed");

        let running = by_id["child-live"];
        assert!(running.live, "one open turn keeps the session live");
        assert!(
            running.last_ended_at.is_none(),
            "an open turn erases the session's end: {running:?}"
        );
        assert_eq!(
            running.first_started_at,
            first.started_at.unwrap(),
            "the clock runs from the EARLIEST turn, not the open one: {running:?}"
        );

        // Unknown ids and an empty request are both empty answers, not errors.
        assert!(store.session_turn_bounds(&[]).await.unwrap().is_empty());
        assert!(
            store
                .session_turn_bounds(&[SessionId::from("nobody")])
                .await
                .unwrap()
                .is_empty()
        );
    }

    async fn load(store: &SqliteTurnStore, id: &TurnId) -> Option<Turn> {
        store
            .get(id)
            .await
            .unwrap()
            .map(|r| Turn::from_row(r).unwrap())
    }

    #[tokio::test]
    async fn create_and_get() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);
        let j = test_turn();
        create(&store, &j).await;
        assert_eq!(load(&store, &j.id).await.unwrap().id, j.id);
    }

    #[tokio::test]
    async fn save_updates_status() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);
        let mut j = test_turn();
        create(&store, &j).await;
        j.start().unwrap();
        store.save(&j.to_row().unwrap()).await.unwrap();
        let loaded = load(&store, &j.id).await.unwrap();
        assert!(matches!(loaded.status, TurnStatus::InProgress));
        assert!(loaded.started_at.is_some());
    }

    #[tokio::test]
    async fn save_nonexistent_returns_not_found() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);
        let j = test_turn();
        let err = store.save(&j.to_row().unwrap()).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_by_session_filters() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);
        create(&store, &test_turn()).await;
        create(&store, &test_turn()).await;
        let turns = store
            .list_by_session(&SessionId::from("sess-1"))
            .await
            .unwrap();
        assert_eq!(turns.len(), 2);
    }

    #[tokio::test]
    async fn list_recoverable_includes_pending_in_progress_stuck() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);
        create(&store, &test_turn()).await;

        let mut in_progress = test_turn();
        in_progress.start().unwrap();
        create(&store, &in_progress).await;

        let mut stuck = test_turn();
        stuck.start().unwrap();
        stuck.stuck("hung").unwrap();
        create(&store, &stuck).await;

        let mut completed = test_turn();
        completed.start().unwrap();
        completed
            .complete(baybo_turn::TurnOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        create(&store, &completed).await;

        let recoverable = store.list_recoverable().await.unwrap();
        assert_eq!(recoverable.len(), 3);
        for r in &recoverable {
            assert_ne!(r.status_kind, "completed");
        }
    }

    #[tokio::test]
    async fn list_active_by_session_scopes_and_filters_status() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);
        let sess_a = SessionId::from("sess-a");
        let sess_b = SessionId::from("sess-b");
        let mk = |s: &SessionId| {
            Turn::new(
                s.clone(),
                TriggerKind::User,
                TurnInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                None,
            )
        };

        // sess-a: pending + in_progress (both active) + completed (terminal).
        create(&store, &mk(&sess_a)).await;
        let mut a_running = mk(&sess_a);
        a_running.start().unwrap();
        create(&store, &a_running).await;
        let mut a_done = mk(&sess_a);
        a_done.start().unwrap();
        a_done
            .complete(baybo_turn::TurnOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        create(&store, &a_done).await;
        // sess-b's in-flight turn must NOT leak into sess-a's results.
        let mut b_running = mk(&sess_b);
        b_running.start().unwrap();
        create(&store, &b_running).await;

        let active = store.list_active_by_session(&sess_a).await.unwrap();
        assert_eq!(active.len(), 2, "only sess-a's non-terminal turns");
        for j in &active {
            assert_eq!(j.session_id, sess_a);
            assert_ne!(j.status_kind, "completed");
        }
    }

    #[tokio::test]
    async fn session_turn_stats_groups_and_picks_latest_status() {
        use std::collections::HashMap;

        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteTurnStore::new(pool);
        let sess_a = SessionId::from("sess-a");
        let sess_b = SessionId::from("sess-b");
        let base = chrono::Utc::now();
        let mk = |s: &SessionId| {
            Turn::new(
                s.clone(),
                TriggerKind::User,
                TurnInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                None,
            )
        };

        // sess-a: completed turn (older) + pending turn (newest) — the
        // stats must report the newest turn's status, not any other.
        let mut a_done = mk(&sess_a);
        a_done.created_at = base;
        a_done.start().unwrap();
        a_done
            .complete(baybo_turn::TurnOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        create(&store, &a_done).await;
        let mut a_pending = mk(&sess_a);
        a_pending.created_at = base + chrono::Duration::seconds(1);
        create(&store, &a_pending).await;

        let mut b_running = mk(&sess_b);
        b_running.created_at = base;
        b_running.start().unwrap();
        create(&store, &b_running).await;

        let stats: HashMap<_, _> = store
            .session_turn_stats()
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.session_id.clone(), s))
            .collect();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[&sess_a].turn_count, 2);
        assert_eq!(stats[&sess_a].latest_status_kind, "pending");
        assert_eq!(stats[&sess_b].turn_count, 1);
        assert_eq!(stats[&sess_b].latest_status_kind, "in_progress");
    }
}
