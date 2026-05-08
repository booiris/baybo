use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use crate::error::StorageError;
use crate::session::{Result, SessionStore};
use aura_model::{ChatMessage, Lineage, LineageKind, Session, SessionId};

pub struct LibsqlSessionStore {
    pool: LibsqlPool,
}

impl LibsqlSessionStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

fn lineage_kind_str(s: &Session) -> Option<&'static str> {
    s.lineage.as_ref().map(|l| match l.kind {
        LineageKind::Subagent => "subagent",
        LineageKind::UserFork { .. } => "user_fork",
    })
}

#[async_trait]
impl SessionStore for LibsqlSessionStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Session>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM sessions WHERE id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let session: Session = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(session)
            .map_err(|e| StorageError::Storage(format!("serialize session: {e}")))?;
        let trigger_kind = match session.trigger.kind() {
            aura_model::TriggerKind::User => "user",
            aura_model::TriggerKind::Cron => "cron",
            aura_model::TriggerKind::System => "system",
            aura_model::TriggerKind::Spawned => "spawned",
        };
        let parent_session = session
            .lineage
            .as_ref()
            .map(|l| l.parent_session_id.as_str().to_string());
        let parent_job = session
            .lineage
            .as_ref()
            .map(|l| l.parent_job_id.to_string());
        let lineage_kind = lineage_kind_str(session).map(|s| s.to_string());
        conn.execute(
            "INSERT OR REPLACE INTO sessions \
             (id, root_session_id, trigger_kind, parent_session_id, parent_job_id, \
              lineage_kind, bound_soul_version, created_at, last_active, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            libsql::params![
                session.id.as_str().to_string(),
                session.root_session_id.as_str().to_string(),
                trigger_kind.to_string(),
                parent_session,
                parent_job,
                lineage_kind,
                session.bound_soul_version.clone(),
                super::time::to_us(session.created_at),
                super::time::to_us(session.last_active),
                data,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql insert session: {e}")))?;
        Ok(())
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        // BEGIN IMMEDIATE acquires a write lock at start, so any concurrent
        // INSERT/UPDATE on `sessions` either blocks behind us or fails BUSY.
        // That's the atomicity contract callers rely on: a fork inserted
        // *after* the live-fork scan returns empty cannot land while we hold
        // the lock.
        let conn = self.pool.conn();
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql begin delete tx: {e}")))?;

        let mut rows = tx
            .query(
                "SELECT id FROM sessions \
                 WHERE parent_session_id = ?1 AND lineage_kind = 'user_fork'",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql live-forks scan: {e}")))?;
        let mut live_forks = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            live_forks.push(SessionId::from(id));
        }
        drop(rows);
        if !live_forks.is_empty() {
            let _ = tx.rollback().await;
            return Err(StorageError::HasLiveForks {
                fork_session_ids: live_forks,
            });
        }

        let affected = tx
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete session: {e}")))?;
        if affected == 0 {
            let _ = tx.rollback().await;
            return Ok(false);
        }
        tx.commit()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql commit: {e}")))?;
        Ok(true)
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<SessionId>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT id FROM sessions WHERE last_active < ?1",
                libsql::params![super::time::to_us(before)],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut expired = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            expired.push(SessionId::from(id));
        }
        Ok(expired)
    }

    async fn list_all(&self) -> Result<Vec<Session>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT data FROM sessions ORDER BY last_active DESC", ())
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut sessions = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let session: Session = serde_json::from_str(&data)
                .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
            sessions.push(session);
        }
        Ok(sessions)
    }

    async fn list_lineage_children(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<(SessionId, LineageKind)>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT id, lineage_kind, data FROM sessions \
                 WHERE parent_session_id = ?1 AND lineage_kind IS NOT NULL",
                libsql::params![parent_session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut children = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let kind_tag: String = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            // Subagent rows carry no extra payload; UserFork rows
            // carry `fork_at_job_id` + `prefix_state_hash` only on
            // the full Lineage struct, which is reconstructable from
            // `data`. Decode `data` only when the kind is UserFork.
            let kind = if kind_tag == "subagent" {
                LineageKind::Subagent
            } else {
                let data: String = row
                    .get(2)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let session: Session = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
                match session.lineage {
                    Some(Lineage { kind, .. }) => kind,
                    None => continue,
                }
            };
            children.push((SessionId::from(id), kind));
        }
        Ok(children)
    }

    async fn list_live_forks(&self, source_session_id: &SessionId) -> Result<Vec<SessionId>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT id FROM sessions \
                 WHERE parent_session_id = ?1 AND lineage_kind = 'user_fork'",
                libsql::params![source_session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut forks = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            forks.push(SessionId::from(id));
        }
        Ok(forks)
    }

    async fn save_context_messages(
        &self,
        session_id: &SessionId,
        messages: &[ChatMessage],
    ) -> Result<()> {
        let conn = self.pool.conn();
        let payload = serde_json::to_string(messages)
            .map_err(|e| StorageError::Storage(format!("serialize context messages: {e}")))?;
        conn.execute(
            "UPDATE sessions SET context_messages = ?2 WHERE id = ?1",
            libsql::params![session_id.as_str().to_string(), payload],
        )
        .await
        .map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql update context_messages: {e}"))
        })?;
        Ok(())
    }

    async fn load_context_messages(&self, session_id: &SessionId) -> Result<Vec<ChatMessage>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT context_messages FROM sessions WHERE id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query context_messages: {e}"))
            })?;

        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?;

        match row {
            Some(row) => {
                let payload: Option<String> = row
                    .get(0)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                match payload {
                    Some(s) if !s.is_empty() => serde_json::from_str(&s).map_err(|e| {
                        StorageError::Storage(format!("deserialize context messages: {e}"))
                    }),
                    _ => Ok(Vec::new()),
                }
            }
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, JobId, Lineage, LineageKind, SessionState, TriggerSource, User};

    fn make_root_session(id: &str) -> Session {
        let id = SessionId::from(id);
        Session {
            id: id.clone(),
            user: User {
                id: "u1".to_string(),
                name: Some("Test".to_string()),
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: SessionState::default(),
            root_session_id: id,
            trigger: TriggerSource::User,
            lineage: None,
            bound_soul_version: "soul-v1".into(),
        }
    }

    fn make_fork_session(id: &str, parent: &SessionId, fork_at: JobId) -> Session {
        let id = SessionId::from(id);
        Session {
            id: id.clone(),
            user: User {
                id: "u1".to_string(),
                name: Some("Test".to_string()),
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: SessionState::default(),
            root_session_id: parent.clone(),
            trigger: TriggerSource::User,
            lineage: Some(Lineage {
                parent_session_id: parent.clone(),
                parent_job_id: fork_at,
                kind: LineageKind::UserFork {
                    fork_at_job_id: fork_at,
                    prefix_state_hash: "hash-1".into(),
                },
            }),
            bound_soul_version: "soul-v1".into(),
        }
    }

    #[tokio::test]
    async fn round_trip_session() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let s = make_root_session("cli-1");
        store.save(&s).await.unwrap();

        let loaded = store.get(&s.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, s.id);
        assert_eq!(loaded.root_session_id, s.id);
        assert_eq!(loaded.bound_soul_version, "soul-v1");

        store.delete(&s.id).await.unwrap();
        assert!(store.get(&s.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_rejects_when_live_forks_exist() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let parent = make_root_session("cli-source");
        store.save(&parent).await.unwrap();

        let fork_at = JobId::new();
        let fork = make_fork_session("cli-fork", &parent.id, fork_at);
        store.save(&fork).await.unwrap();

        let err = store.delete(&parent.id).await.unwrap_err();
        match err {
            StorageError::HasLiveForks { fork_session_ids } => {
                assert_eq!(fork_session_ids, vec![fork.id.clone()]);
            }
            other => panic!("expected HasLiveForks, got {other:?}"),
        }
        // parent must still be live
        assert!(store.get(&parent.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_succeeds_after_fork_deleted() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let parent = make_root_session("cli-source");
        store.save(&parent).await.unwrap();
        let fork_at = JobId::new();
        let fork = make_fork_session("cli-fork", &parent.id, fork_at);
        store.save(&fork).await.unwrap();

        store.delete(&fork.id).await.unwrap();
        // After fork is gone, parent delete must succeed
        store.delete(&parent.id).await.unwrap();
    }

    #[tokio::test]
    async fn list_expired_filters_by_last_active() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let mut old = make_root_session("old");
        old.last_active = Utc::now() - chrono::Duration::hours(2);
        store.save(&old).await.unwrap();
        let new = make_root_session("new");
        store.save(&new).await.unwrap();

        let cutoff = Utc::now() - chrono::Duration::hours(1);
        let expired = store.list_expired(cutoff).await.unwrap();
        assert_eq!(expired, vec![SessionId::from("old")]);
    }
}
