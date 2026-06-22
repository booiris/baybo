//! libsql implementation of [`GoalStore`] — a session's current autonomous
//! objective (`session_goals`).

use async_trait::async_trait;
use aura_model::{Goal, GoalId, GoalStatus, SessionId};

use super::LibsqlPool;
use aura_store::StorageError;
use aura_store::goal::{GoalPatch, GoalStore, Result};

pub struct LibsqlGoalStore {
    pool: LibsqlPool,
}

impl LibsqlGoalStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

const SELECT_COLS: &str = "goal_id, objective, status, token_budget, tokens_used, \
     time_used_seconds, created_at, updated_at";

/// Parse a [`Goal`] from `base` onward. `get` reads from column 0; `list_all`
/// prepends `session_id` and reads from column 1. `None` (with a `warn!`) when
/// the row can't be made into a valid goal — an unknown status from a future
/// variant or a corrupt timestamp. Skipping rather than erroring keeps
/// `list_all` from 500ing on a single bad row, mirroring the task store.
fn try_goal_from_row(row: &libsql::Row, base: i32) -> Option<Goal> {
    let goal_id_s: String = row.get(base).ok()?;
    let objective: String = row.get(base + 1).ok()?;
    let status_s: String = row.get(base + 2).ok()?;
    let token_budget: Option<i64> = row.get(base + 3).ok()?;
    let tokens_used: i64 = row.get(base + 4).ok()?;
    let time_used_seconds: i64 = row.get(base + 5).ok()?;
    let created_at_us: i64 = row.get(base + 6).ok()?;
    let updated_at_us: i64 = row.get(base + 7).ok()?;

    let id = match goal_id_s.parse::<GoalId>() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(goal_id = %goal_id_s, error = %e, "skipping session_goals row: bad goal_id");
            return None;
        }
    };
    let status = match GoalStatus::from_storage_str(&status_s) {
        Some(s) => s,
        None => {
            tracing::warn!(status = %status_s, %id, "skipping session_goals row: unknown status");
            return None;
        }
    };
    let created_at = super::time::from_us(created_at_us)?;
    let updated_at = super::time::from_us(updated_at_us)?;

    Some(Goal {
        id,
        objective,
        status,
        token_budget: token_budget.map(|b| b.max(0) as u64),
        tokens_used: tokens_used.max(0) as u64,
        time_used_seconds: time_used_seconds.max(0) as u64,
        created_at,
        updated_at,
    })
}

#[async_trait]
impl GoalStore for LibsqlGoalStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Goal>> {
        let conn = self.pool.conn();
        let sql = format!("SELECT {SELECT_COLS} FROM session_goals WHERE session_id = ?1");
        let mut rows = conn
            .query(&sql, libsql::params![session_id.as_str().to_string()])
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query session_goals: {e}"))
            })?;
        let row = rows.next().await.map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql session_goals row: {e}"))
        })?;
        Ok(row.and_then(|r| try_goal_from_row(&r, 0)))
    }

    async fn upsert(&self, session_id: &SessionId, goal: &Goal) -> Result<()> {
        let conn = self.pool.conn();
        // Replace the row wholesale: one current goal per session, so a new
        // goal (or a `/goal <obj>` replacing a terminal one) overwrites.
        conn.execute(
            "INSERT INTO session_goals \
                 (session_id, goal_id, objective, status, token_budget, tokens_used, \
                  time_used_seconds, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(session_id) DO UPDATE SET \
                 goal_id           = excluded.goal_id, \
                 objective         = excluded.objective, \
                 status            = excluded.status, \
                 token_budget      = excluded.token_budget, \
                 tokens_used       = excluded.tokens_used, \
                 time_used_seconds = excluded.time_used_seconds, \
                 created_at        = excluded.created_at, \
                 updated_at        = excluded.updated_at",
            libsql::params![
                session_id.as_str().to_string(),
                goal.id.to_string(),
                goal.objective.clone(),
                goal.status.as_str().to_string(),
                goal.token_budget.map(|b| b as i64),
                goal.tokens_used as i64,
                goal.time_used_seconds as i64,
                super::time::to_us(goal.created_at),
                super::time::to_us(goal.updated_at),
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql insert session_goals: {e}")))?;
        Ok(())
    }

    async fn update(&self, session_id: &SessionId, patch: &GoalPatch) -> Result<bool> {
        if patch.is_empty() {
            return Ok(self.get(session_id).await?.is_some());
        }
        let conn = self.pool.conn();
        // Sparse UPDATE touching only the patched columns + updated_at, so the
        // model's `update_goal`, the `/goal` command, and the accounting writes
        // never stomp each other's columns.
        let mut sets: Vec<String> = Vec::new();
        let mut params: Vec<libsql::Value> = Vec::new();
        if let Some(status) = patch.status {
            params.push(libsql::Value::Text(status.as_str().to_string()));
            sets.push(format!("status = ?{}", params.len()));
        }
        if let Some(objective) = &patch.objective {
            params.push(libsql::Value::Text(objective.clone()));
            sets.push(format!("objective = ?{}", params.len()));
        }
        if let Some(token_budget) = patch.token_budget {
            params.push(match token_budget {
                Some(n) => libsql::Value::Integer(n as i64),
                None => libsql::Value::Null,
            });
            sets.push(format!("token_budget = ?{}", params.len()));
        }
        if let Some(tokens_used) = patch.tokens_used {
            params.push(libsql::Value::Integer(tokens_used as i64));
            sets.push(format!("tokens_used = ?{}", params.len()));
        }
        if let Some(time_used_seconds) = patch.time_used_seconds {
            params.push(libsql::Value::Integer(time_used_seconds as i64));
            sets.push(format!("time_used_seconds = ?{}", params.len()));
        }
        params.push(libsql::Value::Integer(super::time::to_us(
            chrono::Utc::now(),
        )));
        sets.push(format!("updated_at = ?{}", params.len()));

        params.push(libsql::Value::Text(session_id.as_str().to_string()));
        let session_idx = params.len();
        let sql = format!(
            "UPDATE session_goals SET {} WHERE session_id = ?{session_idx}",
            sets.join(", ")
        );
        let affected = conn.execute(&sql, params).await.map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql update session_goals: {e}"))
        })?;
        Ok(affected > 0)
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "DELETE FROM session_goals WHERE session_id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql delete session_goals: {e}"))
            })?;
        Ok(affected > 0)
    }

    async fn list_all(&self) -> Result<Vec<(SessionId, Goal)>> {
        let conn = self.pool.conn();
        let sql = format!(
            "SELECT session_id, {SELECT_COLS} FROM session_goals \
             ORDER BY updated_at DESC, goal_id"
        );
        let mut rows = conn.query(&sql, ()).await.map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql list session_goals: {e}"))
        })?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql session_goals row: {e}")))?
        {
            let session_id_s: String = match row.get(0) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "skipping session_goals row: bad session_id");
                    continue;
                }
            };
            // session_id leads; goal columns start at 1.
            if let Some(goal) = try_goal_from_row(&row, 1) {
                out.push((SessionId::from(session_id_s), goal));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libsql::session::LibsqlSessionStore;
    use aura_model::{ChannelType, Session, SessionState, TriggerSource, User};
    use aura_store::SessionStore;
    use chrono::Utc;

    fn make_session(id: &str) -> Session {
        let now = Utc::now();
        Session {
            id: SessionId::from(id),
            user: User {
                id: format!("u-{id}"),
                name: None,
                channel: ChannelType::tui(),
            },
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
        }
    }

    fn now_us() -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap()
    }

    fn make_goal(objective: &str, budget: Option<u64>) -> Goal {
        let now = now_us();
        Goal {
            id: GoalId::new(),
            objective: objective.into(),
            status: GoalStatus::Active,
            token_budget: budget,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: now,
            updated_at: now,
        }
    }

    async fn store_with_session(id: &str) -> (LibsqlGoalStore, SessionId) {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session(id)).await.unwrap();
        (LibsqlGoalStore::new(pool), SessionId::from(id))
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let (store, sid) = store_with_session("g1").await;
        let goal = make_goal("keep refactoring until green", Some(50_000));
        store.upsert(&sid, &goal).await.unwrap();
        let got = store.get(&sid).await.unwrap().unwrap();
        assert_eq!(got, goal);
    }

    #[tokio::test]
    async fn upsert_replaces_current_goal() {
        let (store, sid) = store_with_session("g2").await;
        store.upsert(&sid, &make_goal("first", None)).await.unwrap();
        let second = make_goal("second", Some(10));
        store.upsert(&sid, &second).await.unwrap();
        let got = store.get(&sid).await.unwrap().unwrap();
        assert_eq!(got.objective, "second");
        assert_eq!(got.id, second.id);
    }

    #[tokio::test]
    async fn update_is_sparse() {
        let (store, sid) = store_with_session("g3").await;
        let goal = make_goal("obj", None);
        store.upsert(&sid, &goal).await.unwrap();
        let patch = GoalPatch {
            status: Some(GoalStatus::Paused),
            tokens_used: Some(1234),
            ..GoalPatch::new()
        };
        assert!(store.update(&sid, &patch).await.unwrap());
        let got = store.get(&sid).await.unwrap().unwrap();
        assert_eq!(got.status, GoalStatus::Paused);
        assert_eq!(got.tokens_used, 1234);
        assert_eq!(got.objective, "obj");
        assert!(got.updated_at >= goal.updated_at);
    }

    #[tokio::test]
    async fn update_absent_returns_false() {
        let (store, sid) = store_with_session("g4").await;
        let patch = GoalPatch {
            status: Some(GoalStatus::Complete),
            ..GoalPatch::new()
        };
        assert!(!store.update(&sid, &patch).await.unwrap());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (store, sid) = store_with_session("g5").await;
        store.upsert(&sid, &make_goal("x", None)).await.unwrap();
        assert!(store.delete(&sid).await.unwrap());
        assert!(!store.delete(&sid).await.unwrap());
        assert!(store.get(&sid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_all_spans_sessions() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let sessions = LibsqlSessionStore::new(pool.clone());
        sessions.save(&make_session("a")).await.unwrap();
        sessions.save(&make_session("b")).await.unwrap();
        let store = LibsqlGoalStore::new(pool);
        store
            .upsert(&SessionId::from("a"), &make_goal("goal a", None))
            .await
            .unwrap();
        store
            .upsert(&SessionId::from("b"), &make_goal("goal b", Some(7)))
            .await
            .unwrap();
        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 2);
        let objs: Vec<&str> = all.iter().map(|(_, g)| g.objective.as_str()).collect();
        assert!(objs.contains(&"goal a"));
        assert!(objs.contains(&"goal b"));
    }

    #[tokio::test]
    async fn goal_cascades_delete_with_session() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let sessions = LibsqlSessionStore::new(pool.clone());
        let s = make_session("g-cascade");
        sessions.save(&s).await.unwrap();
        let store = LibsqlGoalStore::new(pool);
        store
            .upsert(&s.id, &make_goal("doomed", None))
            .await
            .unwrap();
        assert!(store.get(&s.id).await.unwrap().is_some());

        sessions.delete(&s.id).await.unwrap();
        assert!(
            store.get(&s.id).await.unwrap().is_none(),
            "the goal must cascade-delete with the parent session"
        );
    }
}
