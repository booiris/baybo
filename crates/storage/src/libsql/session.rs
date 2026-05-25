use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use aura_model::{ChatMessage, Lineage, LineageKind, Session, SessionId};
use aura_store::StorageError;
use aura_store::session::{Result, SessionStore, StoredMessage};

pub struct LibsqlSessionStore {
    pool: LibsqlPool,
}

impl LibsqlSessionStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

/// SQL strings for the `lineage_kind` column, matched by both
/// `lineage_kind_str` (write side) and `list_lineage_children` /
/// `list_active_maintenance_for_parent` (read sides). Keeping them
/// here as named constants prevents drift when a fourth variant lands.
pub(super) const LINEAGE_KIND_SUBAGENT: &str = "subagent";
pub(super) const LINEAGE_KIND_USER_FORK: &str = "user_fork";
pub(super) const LINEAGE_KIND_SYSTEM_MAINTENANCE: &str = "system_maintenance";

fn lineage_kind_str(s: &Session) -> Option<&'static str> {
    s.lineage.as_ref().map(|l| match l.kind {
        LineageKind::Subagent => LINEAGE_KIND_SUBAGENT,
        LineageKind::UserFork { .. } => LINEAGE_KIND_USER_FORK,
        LineageKind::SystemMaintenance => LINEAGE_KIND_SYSTEM_MAINTENANCE,
    })
}

/// Rebuild a typed [`ChatMessage`] from a persisted `session_messages` row,
/// honoring the stored `source` provenance. The sole rehydration seam for the
/// sealed `source` field: every read path funnels its `(role, content, source)`
/// triple here so the `(role, source) -> intent constructor` mapping lives in
/// one place. `User`/`Cron` sources are always `Role::User` (their constructor
/// sets the role), so for those the stored role is redundant and the source
/// wins; an `Agent` source dispatches on the role.
fn rehydrate_message(
    role: &str,
    content: Vec<aura_model::ContentBlock>,
    source: &str,
) -> Result<ChatMessage> {
    use aura_model::{MessageSource, Role};
    let role = role.parse::<Role>().map_err(StorageError::Storage)?;
    let source = source.parse::<MessageSource>().map_err(StorageError::Storage)?;
    Ok(match source {
        MessageSource::User => ChatMessage::user(content),
        MessageSource::UserInterjection => ChatMessage::user_interjection(content),
        MessageSource::Cron => ChatMessage::cron_fire(content),
        MessageSource::Agent => match role {
            Role::User => ChatMessage::agent_context(content),
            Role::Assistant => ChatMessage::assistant(content),
            Role::System => ChatMessage::system(content),
            Role::Tool => ChatMessage::tool(content),
        },
    })
}

/// `is_normal_session` column value: `0` for maintenance sessions
/// (`LineageKind::SystemMaintenance`), `1` otherwise. Default queries
/// filter `is_normal_session = 1` so maintenance sessions stay
/// invisible in regular listings; opt-in helpers query directly.
fn is_normal_session_flag(s: &Session) -> i64 {
    match s.lineage.as_ref().map(|l| &l.kind) {
        Some(LineageKind::SystemMaintenance) => 0,
        _ => 1,
    }
}

#[async_trait]
impl SessionStore for LibsqlSessionStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Session>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data, hidden FROM sessions WHERE id = ?1",
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
                let hidden_col: i64 = row
                    .get(1)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let mut session: Session = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
                // Flat column is authoritative — `set_hidden` updates
                // it directly while leaving the JSON blob untouched
                // to avoid load/save races against `touch`.
                session.hidden = hidden_col != 0;
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
        let parent_span = session
            .lineage
            .as_ref()
            .and_then(|l| l.parent_span_id.as_ref().map(|s| s.to_string()));
        let lineage_kind = lineage_kind_str(session).map(|s| s.to_string());
        let is_normal = is_normal_session_flag(session);
        let hidden_flag: i64 = if session.hidden { 1 } else { 0 };
        // Upsert in place (NOT `INSERT OR REPLACE`, which delete+reinserts
        // the row). The DO UPDATE clause deliberately omits `hidden`:
        // that flat column is owned by `set_hidden`, and `save` carries a
        // possibly-stale in-memory `Session` (e.g. a background-subagent
        // persist after the user hid the conversation). `?12` only seeds
        // `hidden` on a brand-new row; `get` reads the column as
        // authoritative regardless of the blob's stale field.
        conn.execute(
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, parent_session_id, parent_job_id, \
              parent_span_id, lineage_kind, bound_soul_version, created_at, last_active, \
              is_normal_session, hidden, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
             ON CONFLICT(id) DO UPDATE SET \
               root_session_id = excluded.root_session_id, \
               trigger_kind = excluded.trigger_kind, \
               parent_session_id = excluded.parent_session_id, \
               parent_job_id = excluded.parent_job_id, \
               parent_span_id = excluded.parent_span_id, \
               lineage_kind = excluded.lineage_kind, \
               bound_soul_version = excluded.bound_soul_version, \
               created_at = excluded.created_at, \
               last_active = excluded.last_active, \
               is_normal_session = excluded.is_normal_session, \
               data = excluded.data",
            libsql::params![
                session.id.as_str().to_string(),
                session.root_session_id.as_str().to_string(),
                trigger_kind.to_string(),
                parent_session,
                parent_job,
                parent_span,
                lineage_kind,
                session.bound_soul_version.clone(),
                super::time::to_us(session.created_at),
                super::time::to_us(session.last_active),
                is_normal,
                hidden_flag,
                data,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql insert session: {e}")))?;
        Ok(())
    }

    async fn set_hidden(&self, session_id: &SessionId, hidden: bool) -> Result<bool> {
        let conn = self.pool.conn();
        let flag: i64 = if hidden { 1 } else { 0 };
        // Targeted UPDATE on the flat column only — the JSON `data`
        // blob is left alone so a concurrent `touch` (which goes
        // through load + save) can't lose this write. `get` patches
        // `Session.hidden` from the column on read, so observers see
        // the up-to-date value regardless of blob staleness.
        let affected = conn
            .execute(
                "UPDATE sessions SET hidden = ?2 WHERE id = ?1",
                libsql::params![session_id.as_str().to_string(), flag],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql set_hidden: {e}")))?;
        Ok(affected > 0)
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

        // Cascade the message log first — there's no FK in sqlite, so
        // a stranded `session_messages` row would otherwise outlive
        // its parent.
        tx.execute(
            "DELETE FROM session_messages WHERE session_id = ?1",
            libsql::params![session_id.as_str().to_string()],
        )
        .await
        .map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql delete session_messages: {e}"))
        })?;

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
        // Maintenance sessions (`is_normal_session = 0`) are reaped via
        // the startup orphan-marker, not the regular expiry sweep —
        // they're short-lived and stateless by design, and a long-tail
        // expiry from the regular sweep would race with the in-flight
        // pass that owns the row.
        let mut rows = conn
            .query(
                "SELECT id FROM sessions \
                 WHERE last_active < ?1 AND is_normal_session = 1",
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
        // Filter `is_normal_session = 1` so maintenance sessions
        // (e.g. `BackgroundCompression`) stay invisible to user-facing
        // listings (CLI session picker, web UI). Use
        // `list_all_maintenance_sessions` for the maintenance set.
        //
        // Also project the flat `hidden` column — `set_hidden` writes
        // there directly without rewriting the JSON `data` blob, so
        // trusting only the blob would read stale values.
        let mut rows = conn
            .query(
                "SELECT data, hidden FROM sessions \
                 WHERE is_normal_session = 1 \
                 ORDER BY last_active DESC",
                (),
            )
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
            let hidden_col: i64 = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let mut session: Session = serde_json::from_str(&data)
                .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
            session.hidden = hidden_col != 0;
            sessions.push(session);
        }
        Ok(sessions)
    }

    async fn list_by_channel(&self, channel: &aura_model::ChannelType) -> Result<Vec<Session>> {
        // Push the channel filter into SQL via `json_extract` — the
        // sessions table doesn't carry `channel` as a flat column
        // (it rides inside the JSON `data` blob), so a real index
        // isn't available without a schema migration. This is still
        // a full table scan, but non-matching rows never ship their
        // `data` blob over the libsql wire or pay the serde decode
        // in userland, which is the cost we actually care about for
        // a long-running gateway with thousands of bot sessions.
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data, hidden FROM sessions \
                 WHERE is_normal_session = 1 \
                   AND json_extract(data, '$.channel') = ?1 \
                 ORDER BY last_active DESC",
                libsql::params![channel.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query by channel: {e}")))?;

        let mut sessions = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let hidden_col: i64 = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let mut session: Session = serde_json::from_str(&data)
                .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
            session.hidden = hidden_col != 0;
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
            // Subagent and SystemMaintenance carry no extra payload
            // beyond the kind tag, so we can avoid the JSON decode.
            // Only UserFork's variant has fields, and those are only
            // recoverable from the full Lineage struct in `data`.
            let kind = match kind_tag.as_str() {
                LINEAGE_KIND_SUBAGENT => LineageKind::Subagent,
                LINEAGE_KIND_SYSTEM_MAINTENANCE => LineageKind::SystemMaintenance,
                _ => {
                    let data: String = row
                        .get(2)
                        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                    let session: Session = serde_json::from_str(&data)
                        .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
                    match session.lineage {
                        Some(Lineage { kind, .. }) => kind,
                        None => continue,
                    }
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

    async fn list_active_maintenance_for_parent(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<SessionId>> {
        let conn = self.pool.conn();
        let sql = format!(
            "SELECT id FROM sessions \
             WHERE parent_session_id = ?1 \
               AND lineage_kind = '{LINEAGE_KIND_SYSTEM_MAINTENANCE}' \
               AND is_normal_session = 0"
        );
        let mut rows = conn
            .query(
                &sql,
                libsql::params![parent_session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            out.push(SessionId::from(id));
        }
        Ok(out)
    }

    async fn list_all_maintenance_sessions(&self) -> Result<Vec<SessionId>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT id FROM sessions WHERE is_normal_session = 0", ())
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            out.push(SessionId::from(id));
        }
        Ok(out)
    }

    async fn list_unfinished_maintenance_sessions(&self) -> Result<Vec<SessionId>> {
        // Maintenance sessions are reaped only when their associated
        // job is in a non-terminal state — or when there is no job
        // row at all (the session was created but the process died
        // before `with_job` ran). Terminal jobs (completed, failed,
        // cancelled) are kept as audit history so cost reports can
        // still join `cost_records` back through the maintenance
        // session row.
        //
        // String literals match `JobStatusKind`'s `status_kind_str`
        // mapping in `crates/storage/src/libsql/job.rs:18`.
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT s.id FROM sessions s \
                 WHERE s.is_normal_session = 0 \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM jobs j \
                     WHERE j.session_id = s.id \
                     AND j.status_kind IN ('completed', 'failed', 'cancelled') \
                 )",
                (),
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            out.push(SessionId::from(id));
        }
        Ok(out)
    }

    async fn append_session_message(
        &self,
        session_id: &SessionId,
        message: &ChatMessage,
    ) -> Result<i64> {
        let conn = self.pool.conn();
        let role = message.role.as_str();
        let content = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::Storage(format!("serialize message content: {e}")))?;
        let now_us = super::time::to_us(chrono::Utc::now());
        // `INSERT … SELECT COALESCE(MAX(ordinal),-1)+1 … RETURNING` keeps
        // ordinals contiguous without an explicit sequence and hands
        // back the assigned value in one round trip. The actor model
        // serialises writes per session, so there's no concurrent-
        // append race to defend against here.
        let mut rows = conn
            .query(
                "INSERT INTO session_messages \
             (session_id, ordinal, role, content, created_at, source) \
             SELECT ?1, COALESCE(MAX(ordinal), -1) + 1, ?2, ?3, ?4, ?5 \
             FROM session_messages WHERE session_id = ?1 \
             RETURNING ordinal",
                libsql::params![
                    session_id.as_str().to_string(),
                    role.to_string(),
                    content,
                    now_us,
                    message.source().as_str().to_string(),
                ],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql append session_message: {e}"))
            })?;
        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
            .ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!(
                    "INSERT … RETURNING returned no rows for session_messages"
                ))
            })?;
        let ordinal: i64 = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get ordinal: {e}")))?;
        Ok(ordinal)
    }

    async fn apply_session_compaction(
        &self,
        session_id: &SessionId,
        new_active: &[ChatMessage],
    ) -> Result<()> {
        let conn = self.pool.conn();
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql begin compaction tx: {e}"))
            })?;

        // Next ordinal doubles as the supersede pointer: every
        // existing active row points at it, and the first new active
        // message lands there.
        let mut rows = tx
            .query(
                "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql max ordinal: {e}")))?;
        let next_ordinal: i64 = match rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            Some(row) => row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?,
            None => 0,
        };
        drop(rows);

        tx.execute(
            "UPDATE session_messages SET superseded_by = ?2 \
             WHERE session_id = ?1 AND superseded_by IS NULL",
            libsql::params![session_id.as_str().to_string(), next_ordinal],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql supersede: {e}")))?;

        let now_us = super::time::to_us(chrono::Utc::now());
        // Multi-row INSERT, batched under SQLite's 999-bind limit.
        // 6 columns per row → 166 rows per batch leaves 3 spare;
        // typical Summarize emits ≤4 rows so this is one batch in
        // practice. Keeps the whole compaction inside one tx and
        // round-trip count constant (1) instead of O(new_active).
        const COLS_PER_ROW: usize = 6;
        const ROWS_PER_BATCH: usize = 999 / COLS_PER_ROW;
        let session_param = session_id.as_str().to_string();
        for (chunk_idx, chunk) in new_active.chunks(ROWS_PER_BATCH).enumerate() {
            let mut sql = String::from(
                "INSERT INTO session_messages \
                 (session_id, ordinal, role, content, created_at, source) VALUES ",
            );
            let mut params: Vec<libsql::Value> = Vec::with_capacity(chunk.len() * COLS_PER_ROW);
            for (i, msg) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let p = i * COLS_PER_ROW;
                sql.push_str(&format!(
                    "(?{}, ?{}, ?{}, ?{}, ?{}, ?{})",
                    p + 1,
                    p + 2,
                    p + 3,
                    p + 4,
                    p + 5,
                    p + 6
                ));
                let ordinal = next_ordinal + (chunk_idx * ROWS_PER_BATCH) as i64 + i as i64;
                let content = serde_json::to_string(&msg.content).map_err(|e| {
                    StorageError::Storage(format!("serialize message content: {e}"))
                })?;
                params.push(libsql::Value::Text(session_param.clone()));
                params.push(libsql::Value::Integer(ordinal));
                params.push(libsql::Value::Text(msg.role.as_str().to_string()));
                params.push(libsql::Value::Text(content));
                params.push(libsql::Value::Integer(now_us));
                params.push(libsql::Value::Text(msg.source().as_str().to_string()));
            }
            tx.execute(&sql, params).await.map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql compaction insert: {e}"))
            })?;
        }

        tx.commit().await.map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql commit compaction: {e}"))
        })?;
        Ok(())
    }

    async fn load_active_session_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ChatMessage>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT role, content, source FROM session_messages \
                 WHERE session_id = ?1 AND superseded_by IS NULL \
                 ORDER BY ordinal",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query session_messages: {e}"))
            })?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let role: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get role: {e}")))?;
            let content_json: String = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get content: {e}")))?;
            let source_str: String = row.get(2).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get source: {e}"))
            })?;
            let content = serde_json::from_str(&content_json)
                .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
            out.push(rehydrate_message(&role, content, &source_str)?);
        }
        Ok(out)
    }

    async fn latest_session_ordinal(&self, session_id: &SessionId) -> Result<Option<i64>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT MAX(ordinal) FROM session_messages WHERE session_id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql max ordinal: {e}")))?;
        match rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            Some(row) => Ok(row
                .get::<Option<i64>>(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?),
            None => Ok(None),
        }
    }

    async fn active_index_of_ordinal(
        &self,
        session_id: &SessionId,
        ordinal: i64,
    ) -> Result<Option<usize>> {
        let conn = self.pool.conn();
        // Both sub-selects hit `idx_session_messages_active`
        // (`session_id, ordinal WHERE superseded_by IS NULL`), so this
        // is two index-only counts — the row content is never read.
        let mut rows = conn
            .query(
                "SELECT \
                   (SELECT COUNT(*) FROM session_messages \
                    WHERE session_id = ?1 AND superseded_by IS NULL AND ordinal < ?2), \
                   EXISTS (SELECT 1 FROM session_messages \
                           WHERE session_id = ?1 AND superseded_by IS NULL AND ordinal = ?2)",
                libsql::params![session_id.as_str().to_string(), ordinal],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql active_index query: {e}"))
            })?;
        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
            .ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!("active_index returned no rows"))
            })?;
        let count: i64 = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get count: {e}")))?;
        let present: i64 = row
            .get(1)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get present: {e}")))?;
        if present == 0 {
            return Ok(None);
        }
        Ok(Some(count as usize))
    }

    async fn count_active_messages(&self, session_id: &SessionId) -> Result<usize> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM session_messages \
                 WHERE session_id = ?1 AND superseded_by IS NULL",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql count_active query: {e}"))
            })?;
        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
            .ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!("count_active returned no rows"))
            })?;
        let count: i64 = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get count: {e}")))?;
        Ok(count as usize)
    }

    async fn load_active_session_messages_up_to(
        &self,
        session_id: &SessionId,
        up_to_ordinal: i64,
    ) -> Result<Vec<ChatMessage>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT role, content, source FROM session_messages \
                 WHERE session_id = ?1 AND superseded_by IS NULL AND ordinal <= ?2 \
                 ORDER BY ordinal",
                libsql::params![session_id.as_str().to_string(), up_to_ordinal],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query active up_to: {e}"))
            })?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let role: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get role: {e}")))?;
            let content_json: String = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get content: {e}")))?;
            let source_str: String = row.get(2).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get source: {e}"))
            })?;
            let content = serde_json::from_str(&content_json)
                .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
            out.push(rehydrate_message(&role, content, &source_str)?);
        }
        Ok(out)
    }

    async fn load_active_session_messages_tail(
        &self,
        session_id: &SessionId,
        before_ordinal: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, aura_model::ChatMessage)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.pool.conn();
        // `before_ordinal IS NULL OR ordinal < before_ordinal` so a
        // single SQL string handles both the "fresh tail" and the
        // scroll-up "next page" calls. The partial active index
        // (`session_id, ordinal WHERE superseded_by IS NULL`) makes
        // the DESC+LIMIT a back-of-the-index walk — never reads more
        // than `limit` row contents off disk even on a million-row
        // session.
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = conn
            .query(
                "SELECT ordinal, role, content, source, created_at FROM session_messages \
                 WHERE session_id = ?1 AND superseded_by IS NULL \
                   AND (?2 IS NULL OR ordinal < ?2) \
                 ORDER BY ordinal DESC \
                 LIMIT ?3",
                libsql::params![session_id.as_str().to_string(), before_ordinal, limit_i64,],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query active tail: {e}"))
            })?;

        let mut out: Vec<(i64, DateTime<Utc>, aura_model::ChatMessage)> = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let ordinal: i64 = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get ord: {e}")))?;
            let role: String = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get role: {e}")))?;
            let content_json: String = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get content: {e}")))?;
            let source_str: String = row.get(3).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get source: {e}"))
            })?;
            let created_us: i64 = row.get(4).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get created_at: {e}"))
            })?;
            let created_at = super::time::from_us(created_us).ok_or_else(|| {
                StorageError::Storage(format!(
                    "session_messages.created_at out of range: {created_us}"
                ))
            })?;
            let content = serde_json::from_str(&content_json)
                .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
            out.push((
                ordinal,
                created_at,
                rehydrate_message(&role, content, &source_str)?,
            ));
        }
        // Caller expects ascending ordinal order — the SQL pulled the
        // newest rows first so the LIMIT bites the tail rather than the
        // head, but the consumer renders top-to-bottom.
        out.reverse();
        Ok(out)
    }

    async fn load_active_session_messages_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        limit: usize,
    ) -> Result<Vec<(i64, aura_model::ChatMessage)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.pool.conn();
        // Forward catch-up: rows with ordinal strictly greater than the
        // client's cursor, capped at `limit`. The partial active index
        // bites the front of the range (`ordinal > N`) so a session
        // with hundreds of older rows pays nothing for them — only the
        // catch-up window is read.
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = conn
            .query(
                "SELECT ordinal, role, content, source FROM session_messages \
                 WHERE session_id = ?1 AND superseded_by IS NULL \
                   AND ordinal > ?2 \
                 ORDER BY ordinal ASC \
                 LIMIT ?3",
                libsql::params![session_id.as_str().to_string(), after_ordinal, limit_i64,],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query active since: {e}"))
            })?;

        let mut out: Vec<(i64, aura_model::ChatMessage)> = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let ordinal: i64 = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get ord: {e}")))?;
            let role: String = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get role: {e}")))?;
            let content_json: String = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get content: {e}")))?;
            let source_str: String = row.get(3).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get source: {e}"))
            })?;
            let content = serde_json::from_str(&content_json)
                .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
            out.push((
                ordinal,
                rehydrate_message(&role, content, &source_str)?,
            ));
        }
        Ok(out)
    }

    async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredMessage>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT ordinal, superseded_by, role, content, created_at, source \
                 FROM session_messages \
                 WHERE session_id = ?1 ORDER BY ordinal",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query session_messages: {e}"))
            })?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let ordinal: i64 = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get ord: {e}")))?;
            let superseded_by: Option<i64> = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get sup: {e}")))?;
            let role: String = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get role: {e}")))?;
            let content_json: String = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get content: {e}")))?;
            let created_us: i64 = row
                .get(4)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get created: {e}")))?;
            let source_str: String = row.get(5).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get source: {e}"))
            })?;
            let created_at = super::time::from_us(created_us).ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!(
                    "session_messages.created_at out of range: {created_us}"
                ))
            })?;
            let content = serde_json::from_str(&content_json)
                .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
            out.push(StoredMessage {
                ordinal,
                superseded_by,
                created_at,
                message: rehydrate_message(&role, content, &source_str)?,
            });
        }
        Ok(out)
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
            hidden: false,
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
                parent_span_id: None,
                kind: LineageKind::UserFork {
                    fork_at_job_id: fork_at,
                    prefix_state_hash: "hash-1".into(),
                },
            }),
            bound_soul_version: "soul-v1".into(),
            hidden: false,
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

    #[tokio::test]
    async fn list_by_channel_pushes_predicate_into_sql() {
        // Mixed-channel fixture: two http sessions, one telegram. The
        // chat REST surface only wants http; the push-down keeps
        // telegram rows out of the libsql round-trip entirely so a
        // gateway hosting thousands of bot sessions doesn't pay an
        // O(all-sessions) cost on every chat-list refresh.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let mut http_a = make_root_session("http-a");
        http_a.channel = ChannelType::http();
        store.save(&http_a).await.unwrap();

        let mut tg = make_root_session("tg-1");
        tg.channel = ChannelType::telegram();
        store.save(&tg).await.unwrap();

        let mut http_b = make_root_session("http-b");
        http_b.channel = ChannelType::http();
        store.save(&http_b).await.unwrap();

        let http = store.list_by_channel(&ChannelType::http()).await.unwrap();
        let http_ids: Vec<&str> = http.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(http_ids.len(), 2);
        assert!(http_ids.contains(&"http-a"));
        assert!(http_ids.contains(&"http-b"));
        assert!(!http_ids.contains(&"tg-1"));

        let telegram = store
            .list_by_channel(&ChannelType::telegram())
            .await
            .unwrap();
        assert_eq!(telegram.len(), 1);
        assert_eq!(telegram[0].id.as_str(), "tg-1");

        // A channel with no rows comes back empty, not as an error.
        let empty = store.list_by_channel(&ChannelType::weixin()).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn list_by_channel_reflects_hidden_column() {
        // `list_by_channel` must project the flat `hidden` column the
        // same way `list_all` does — `set_hidden` writes only that
        // column without rewriting the JSON `data` blob, so trusting
        // the deserialised `Session.hidden` alone would read stale
        // values.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let mut s = make_root_session("hide-me");
        s.channel = ChannelType::http();
        store.save(&s).await.unwrap();
        assert!(store.set_hidden(&s.id, true).await.unwrap());

        let listed = store.list_by_channel(&ChannelType::http()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].hidden, "hidden flag must reflect the column");
    }

    #[tokio::test]
    async fn save_does_not_clobber_hidden_set_by_set_hidden() {
        // A background-subagent persist saves the actor's in-memory
        // `Session` (hidden=false) AFTER the user hid the conversation
        // via `set_hidden`. `save` must not rewrite the flat `hidden`
        // column, or it would silently un-hide the row.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let s = make_root_session("hide-then-save");
        store.save(&s).await.unwrap();
        assert!(store.set_hidden(&s.id, true).await.unwrap());

        // Re-save the stale in-memory copy (still hidden=false).
        store.save(&s).await.unwrap();

        let loaded = store.get(&s.id).await.unwrap().expect("row present");
        assert!(
            loaded.hidden,
            "save must preserve the hidden column owned by set_hidden"
        );
    }

    #[tokio::test]
    async fn load_active_session_messages_tail_paginates_reverse() {
        // Seven user messages on one session, ordinals 0..=6. The
        // tail loader is the path the chat REST surface uses to ship
        // a long-running session's transcript a page at a time
        // without fetching the whole row stream up-front.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let session = make_root_session("paginate-me");
        store.save(&session).await.unwrap();
        for i in 0..7 {
            let msg = aura_model::ChatMessage::user(vec![aura_model::ContentBlock::Text(format!(
                "msg-{i}"
            ))]);
            store
                .append_session_message(&session.id, &msg)
                .await
                .unwrap();
        }

        // Tail page: last 3 messages, ordinals 4..=6 in ascending order.
        let tail = store
            .load_active_session_messages_tail(&session.id, None, 3)
            .await
            .unwrap();
        let ordinals: Vec<i64> = tail.iter().map(|(o, _, _)| *o).collect();
        assert_eq!(ordinals, vec![4, 5, 6]);

        // Scroll-up page: the 3 messages strictly before ordinal 4,
        // i.e. 1..=3 in ascending order.
        let older = store
            .load_active_session_messages_tail(&session.id, Some(4), 3)
            .await
            .unwrap();
        let older_ords: Vec<i64> = older.iter().map(|(o, _, _)| *o).collect();
        assert_eq!(older_ords, vec![1, 2, 3]);

        // Final page: only ordinal 0 is older than 1, so a `limit=3`
        // request returns a single row (not three).
        let head = store
            .load_active_session_messages_tail(&session.id, Some(1), 3)
            .await
            .unwrap();
        let head_ords: Vec<i64> = head.iter().map(|(o, _, _)| *o).collect();
        assert_eq!(head_ords, vec![0]);

        // Beyond the start: empty result, no error.
        let empty = store
            .load_active_session_messages_tail(&session.id, Some(0), 3)
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn load_active_session_messages_since_forward_pages_above_cursor() {
        // Same 7-row fixture as the `_tail` test, but exercising the
        // catch-up cursor path the WS `Subscribe { since_ordinal }`
        // route uses to replay missed rows to a reconnecting client.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let session = make_root_session("catch-up-me");
        store.save(&session).await.unwrap();
        for i in 0..7 {
            let msg = aura_model::ChatMessage::user(vec![aura_model::ContentBlock::Text(format!(
                "msg-{i}"
            ))]);
            store
                .append_session_message(&session.id, &msg)
                .await
                .unwrap();
        }

        // From cursor 3, ask for up to 10: every row with ordinal > 3,
        // ascending — i.e. 4, 5, 6.
        let after_3 = store
            .load_active_session_messages_since(&session.id, 3, 10)
            .await
            .unwrap();
        let ords: Vec<i64> = after_3.iter().map(|(o, _)| *o).collect();
        assert_eq!(ords, vec![4, 5, 6]);

        // `limit` is the cap: from cursor 0, ask for 2 — the first
        // two missed rows (1, 2), not the whole tail.
        let cap = store
            .load_active_session_messages_since(&session.id, 0, 2)
            .await
            .unwrap();
        let cap_ords: Vec<i64> = cap.iter().map(|(o, _)| *o).collect();
        assert_eq!(cap_ords, vec![1, 2]);

        // Caught up: the cursor is at (or past) the latest row, so
        // the slice is empty. `limit + 1` over-fetch sees zero ⇒
        // server's "no catch-up needed" branch.
        let none = store
            .load_active_session_messages_since(&session.id, 6, 10)
            .await
            .unwrap();
        assert!(none.is_empty());

        // From before the first row: returns every row in order.
        let all = store
            .load_active_session_messages_since(&session.id, -1, 10)
            .await
            .unwrap();
        let all_ords: Vec<i64> = all.iter().map(|(o, _)| *o).collect();
        assert_eq!(all_ords, vec![0, 1, 2, 3, 4, 5, 6]);
    }
}
