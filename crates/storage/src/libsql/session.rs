use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use baybo_model::{
    ChatMessage, ControlEvent, ControlEventKind, FolderId, LineageKind, LlmEntryName, Session,
    SessionId,
};
use baybo_store::StorageError;
use baybo_store::session::{Result, SessionStore, StoredMessage};

pub struct LibsqlSessionStore {
    pool: LibsqlPool,
}

impl LibsqlSessionStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

/// SQL string for the `lineage_kind` column, matched by both
/// `lineage_kind_str` (write side) and `list_lineage_children`
/// (read side). Keeping it here as a named constant prevents drift
/// when a new variant lands.
pub(super) const LINEAGE_KIND_SUBAGENT: &str = "subagent";

fn lineage_kind_str(s: &Session) -> Option<&'static str> {
    s.lineage.as_ref().map(|l| match l.kind {
        LineageKind::Subagent => LINEAGE_KIND_SUBAGENT,
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
    content: Vec<baybo_model::ContentBlock>,
    source: &str,
    platform_msg_id: String,
) -> Result<ChatMessage> {
    use baybo_model::{MessageSource, Role};
    let role = role.parse::<Role>().map_err(StorageError::Storage)?;
    let source = source
        .parse::<MessageSource>()
        .map_err(StorageError::Storage)?;
    Ok(match source {
        MessageSource::User => ChatMessage::user(content),
        MessageSource::UserInterjection => ChatMessage::user_interjection(content),
        MessageSource::Cron => ChatMessage::cron_fire(content),
        MessageSource::RecalledMemory => ChatMessage::recalled_memory(content),
        MessageSource::Agent => match role {
            Role::User => ChatMessage::agent_context(content),
            Role::Assistant => ChatMessage::assistant(content),
            Role::System => ChatMessage::system(content),
            Role::Tool => ChatMessage::tool(content),
        },
    }
    .with_platform_msg_id(platform_msg_id))
}

#[async_trait]
impl SessionStore for LibsqlSessionStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Session>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data, hidden, last_llm, pinned, folder_id, title FROM sessions WHERE id = ?1",
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
                let last_llm_col: Option<String> = row
                    .get(2)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let pinned_col: i64 = row
                    .get(3)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let folder_id_col: Option<String> = row
                    .get(4)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let title_col: Option<String> = row
                    .get(5)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let mut session: Session = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
                // Flat columns are authoritative; targeted setters leave the
                // JSON blob untouched to avoid load/save races.
                session.hidden = hidden_col != 0;
                session.state.last_llm = last_llm_col.map(LlmEntryName::from);
                session.pinned = pinned_col != 0;
                session.folder_id = folder_id_col.map(FolderId::from);
                session.title = title_col;
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
            baybo_model::TriggerKind::User => "user",
            baybo_model::TriggerKind::Cron => "cron",
            baybo_model::TriggerKind::System => "system",
            baybo_model::TriggerKind::Spawned => "spawned",
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
        let hidden_flag: i64 = if session.hidden { 1 } else { 0 };
        let pinned_flag: i64 = if session.pinned { 1 } else { 0 };
        // Upsert in place (NOT `INSERT OR REPLACE`, which delete+reinserts
        // the row). The DO UPDATE clause omits flat columns owned by targeted
        // setters (`hidden`, `last_llm`, `pinned`, `folder_id`, `title`) so a
        // stale in-memory `Session` cannot clobber them. `hidden` / `pinned`
        // are seeded only on a brand-new row; `get` reads all flat columns as
        // authoritative over the JSON blob.
        conn.execute(
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, parent_session_id, parent_job_id, \
              parent_span_id, lineage_kind, created_at, last_active, \
              hidden, pinned, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(id) DO UPDATE SET \
               root_session_id = excluded.root_session_id, \
               trigger_kind = excluded.trigger_kind, \
               parent_session_id = excluded.parent_session_id, \
               parent_job_id = excluded.parent_job_id, \
               parent_span_id = excluded.parent_span_id, \
               lineage_kind = excluded.lineage_kind, \
               created_at = excluded.created_at, \
               last_active = excluded.last_active, \
               data = excluded.data",
            libsql::params![
                session.id.as_str().to_string(),
                session.root_session_id.as_str().to_string(),
                trigger_kind.to_string(),
                parent_session,
                parent_job,
                parent_span,
                lineage_kind,
                super::time::to_us(session.created_at),
                super::time::to_us(session.last_active),
                hidden_flag,
                pinned_flag,
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

    async fn set_last_llm(
        &self,
        session_id: &SessionId,
        llm: Option<&LlmEntryName>,
    ) -> Result<bool> {
        let conn = self.pool.conn();
        // Targeted UPDATE on the flat column only — like `set_hidden`,
        // the JSON `data` blob is left alone so a concurrent `touch`
        // (load + full save) can't lose this write. `get` patches
        // `Session.state.last_llm` from the column on read. `NULL`
        // clears the pin back to `default-llm`.
        let value: Option<String> = llm.map(|n| n.as_str().to_string());
        let affected = conn
            .execute(
                "UPDATE sessions SET last_llm = ?2 WHERE id = ?1",
                libsql::params![session_id.as_str().to_string(), value],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql set_last_llm: {e}")))?;
        Ok(affected > 0)
    }

    async fn set_pinned(&self, session_id: &SessionId, pinned: bool) -> Result<bool> {
        let conn = self.pool.conn();
        let flag: i64 = if pinned { 1 } else { 0 };
        // Targeted UPDATE on the flat column only — like `set_hidden`,
        // the JSON `data` blob is left alone so a concurrent `touch`
        // (load + full save) can't lose this write. `get` patches
        // `Session.pinned` from the column on read.
        let affected = conn
            .execute(
                "UPDATE sessions SET pinned = ?2 WHERE id = ?1",
                libsql::params![session_id.as_str().to_string(), flag],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql set_pinned: {e}")))?;
        Ok(affected > 0)
    }

    async fn set_folder(
        &self,
        session_id: &SessionId,
        folder_id: Option<&FolderId>,
    ) -> Result<bool> {
        let conn = self.pool.conn();
        // Targeted UPDATE on the flat column only — like `set_hidden` /
        // `set_pinned`, the JSON `data` blob is left alone so a concurrent
        // `touch` (load + full save) can't lose this write. `get` patches
        // `Session.folder_id` from the column on read. `NULL` clears the
        // assignment back to uncategorized.
        let value: Option<String> = folder_id.map(|f| f.as_str().to_string());
        let affected = conn
            .execute(
                "UPDATE sessions SET folder_id = ?2 WHERE id = ?1",
                libsql::params![session_id.as_str().to_string(), value],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql set_folder: {e}")))?;
        Ok(affected > 0)
    }

    async fn set_read_cursor(&self, session_id: &SessionId, ordinal: i64) -> Result<bool> {
        let conn = self.pool.conn();
        // Targeted, max-wins UPDATE on the flat column only — the `CASE`
        // guards against a reordered/stale marker regressing the cursor (a
        // background tab PUTting an older read position must not undo a newer
        // one). The JSON `data` blob is untouched, like `set_pinned`.
        let affected = conn
            .execute(
                "UPDATE sessions \
                 SET read_cursor = CASE \
                     WHEN read_cursor IS NULL OR ?2 > read_cursor THEN ?2 \
                     ELSE read_cursor END \
                 WHERE id = ?1",
                libsql::params![session_id.as_str().to_string(), ordinal],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql set_read_cursor: {e}")))?;
        Ok(affected > 0)
    }

    async fn read_cursor(&self, session_id: &SessionId) -> Result<Option<i64>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT read_cursor FROM sessions WHERE id = ?1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql read_cursor: {e}")))?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        else {
            return Ok(None);
        };
        row.get::<Option<i64>>(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get read_cursor: {e}")))
    }

    async fn set_title(&self, session_id: &SessionId, title: Option<&str>) -> Result<bool> {
        let conn = self.pool.conn();
        let value: Option<String> = title.map(|t| t.to_string());
        let affected = conn
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                libsql::params![session_id.as_str().to_string(), value],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql set_title: {e}")))?;
        Ok(affected > 0)
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        // The message-log cascade and the session-row delete must commit
        // as a unit (see below); BEGIN IMMEDIATE takes the write lock up
        // front so the pair runs without an interleaved writer.
        let conn = self.pool.conn();
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql begin delete tx: {e}")))?;

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
        // Project the flat `hidden` column — `set_hidden` writes there
        // directly without rewriting the JSON `data` blob, so trusting
        // only the blob would read stale values. `id` rides along purely
        // so a row whose `data` blob fails to deserialize (e.g. one
        // written by an older build whose lineage kind this build doesn't
        // know) can be named in the skip warning.
        let mut rows = conn
            .query(
                "SELECT data, hidden, last_llm, pinned, id, folder_id, title FROM sessions \
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
            let last_llm_col: Option<String> = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let pinned_col: i64 = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let id_col: String = row
                .get(4)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let folder_id_col: Option<String> = row
                .get(5)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let title_col: Option<String> = row
                .get(6)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            // A single undeserializable row (e.g. one written by an older
            // build whose `lineage.kind` this build doesn't know) must
            // degrade to "silently absent from the listing", never fail
            // the whole listing and 500 the CLI picker / web UI.
            let mut session: Session = match serde_json::from_str(&data) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        session_id = %id_col,
                        "skipping session row that failed to deserialize: {e}"
                    );
                    continue;
                }
            };
            session.hidden = hidden_col != 0;
            session.state.last_llm = last_llm_col.map(LlmEntryName::from);
            session.pinned = pinned_col != 0;
            session.folder_id = folder_id_col.map(FolderId::from);
            session.title = title_col;
            sessions.push(session);
        }
        Ok(sessions)
    }

    async fn list_by_channel(&self, channel: &baybo_model::ChannelType) -> Result<Vec<Session>> {
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
                "SELECT data, hidden, last_llm, pinned, id, folder_id, title FROM sessions \
                 WHERE json_extract(data, '$.channel') = ?1 \
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
            let last_llm_col: Option<String> = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let pinned_col: i64 = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let id_col: String = row
                .get(4)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let folder_id_col: Option<String> = row
                .get(5)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let title_col: Option<String> = row
                .get(6)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            // Same skip-on-error discipline as `list_all`: a row whose
            // blob fails to deserialize drops out of the listing rather
            // than failing the whole chat-list query.
            let mut session: Session = match serde_json::from_str(&data) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        session_id = %id_col,
                        "skipping session row that failed to deserialize: {e}"
                    );
                    continue;
                }
            };
            session.hidden = hidden_col != 0;
            session.state.last_llm = last_llm_col.map(LlmEntryName::from);
            session.pinned = pinned_col != 0;
            session.folder_id = folder_id_col.map(FolderId::from);
            session.title = title_col;
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
                "SELECT id, lineage_kind FROM sessions \
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
            // The variant is payload-free, so the kind tag alone
            // reconstructs the `LineageKind` — no JSON decode needed.
            // An unrecognised tag (e.g. an orphaned legacy
            // `system_maintenance` row) is skipped rather than erroring
            // the whole listing.
            let kind = match kind_tag.as_str() {
                LINEAGE_KIND_SUBAGENT => LineageKind::Subagent,
                _ => continue,
            };
            children.push((SessionId::from(id), kind));
        }
        Ok(children)
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
             (session_id, ordinal, role, content, created_at, source, platform_msg_id) \
             SELECT ?1, COALESCE(MAX(ordinal), -1) + 1, ?2, ?3, ?4, ?5, ?6 \
             FROM session_messages WHERE session_id = ?1 \
             RETURNING ordinal",
                libsql::params![
                    session_id.as_str().to_string(),
                    role.to_string(),
                    content,
                    now_us,
                    message.source().as_str().to_string(),
                    message.platform_msg_id().to_string(),
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

    async fn append_control_event(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        created_at: DateTime<Utc>,
    ) -> Result<i64> {
        let conn = self.pool.conn();
        let created_us = super::time::to_us(created_at);
        let mut rows = conn
            .query(
                "INSERT INTO session_control_events \
             (session_id, seq, after_ordinal, kind, text, created_at) \
             SELECT ?1, COALESCE(MAX(seq), -1) + 1, ?2, ?3, ?4, ?5 \
             FROM session_control_events WHERE session_id = ?1 \
             RETURNING seq",
                libsql::params![
                    session_id.as_str().to_string(),
                    after_ordinal,
                    kind.as_str().to_string(),
                    text.to_string(),
                    created_us,
                ],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql append control event: {e}"))
            })?;
        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
            .ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!(
                    "INSERT … RETURNING returned no rows for session_control_events"
                ))
            })?;
        let seq: i64 = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get seq: {e}")))?;
        Ok(seq)
    }

    async fn list_control_events(&self, session_id: &SessionId) -> Result<Vec<ControlEvent>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT seq, after_ordinal, kind, text, created_at FROM session_control_events \
                 WHERE session_id = ?1 ORDER BY seq",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql list control events: {e}"))
            })?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let seq: i64 = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get seq: {e}")))?;
            let after_ordinal: i64 = row.get(1).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get after_ordinal: {e}"))
            })?;
            let kind_str: String = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get kind: {e}")))?;
            let text: String = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get text: {e}")))?;
            let created_us: i64 = row.get(4).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get created_at: {e}"))
            })?;
            let kind = kind_str
                .parse::<ControlEventKind>()
                .map_err(StorageError::Storage)?;
            let created_at = super::time::from_us(created_us).ok_or_else(|| {
                StorageError::Storage(format!(
                    "session_control_events.created_at out of range: {created_us}"
                ))
            })?;
            out.push(ControlEvent {
                seq,
                after_ordinal,
                kind,
                text,
                created_at,
            });
        }
        Ok(out)
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
        // 7 columns per row → 142 rows per batch leaves 5 spare;
        // typical Summarize emits ≤4 rows so this is one batch in
        // practice. Keeps the whole compaction inside one tx and
        // round-trip count constant (1) instead of O(new_active).
        const COLS_PER_ROW: usize = 7;
        const ROWS_PER_BATCH: usize = 999 / COLS_PER_ROW;
        let session_param = session_id.as_str().to_string();
        for (chunk_idx, chunk) in new_active.chunks(ROWS_PER_BATCH).enumerate() {
            let mut sql = String::from(
                "INSERT INTO session_messages \
                 (session_id, ordinal, role, content, created_at, source, platform_msg_id) VALUES ",
            );
            let mut params: Vec<libsql::Value> = Vec::with_capacity(chunk.len() * COLS_PER_ROW);
            for (i, msg) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let p = i * COLS_PER_ROW;
                sql.push_str(&format!(
                    "(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{})",
                    p + 1,
                    p + 2,
                    p + 3,
                    p + 4,
                    p + 5,
                    p + 6,
                    p + 7
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
                params.push(libsql::Value::Text(msg.platform_msg_id().to_string()));
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
                "SELECT role, content, source, platform_msg_id FROM session_messages \
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
            let source_str: String = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get source: {e}")))?;
            let platform_msg_id: String = row.get(3).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get platform_msg_id: {e}"))
            })?;
            let content = serde_json::from_str(&content_json)
                .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
            out.push(rehydrate_message(
                &role,
                content,
                &source_str,
                platform_msg_id,
            )?);
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
                "SELECT role, content, source, platform_msg_id FROM session_messages \
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
            let source_str: String = row
                .get(2)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get source: {e}")))?;
            let platform_msg_id: String = row.get(3).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get platform_msg_id: {e}"))
            })?;
            let content = serde_json::from_str(&content_json)
                .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
            out.push(rehydrate_message(
                &role,
                content,
                &source_str,
                platform_msg_id,
            )?);
        }
        Ok(out)
    }

    async fn load_active_session_messages_tail(
        &self,
        session_id: &SessionId,
        before_ordinal: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, baybo_model::ChatMessage)>> {
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
                "SELECT ordinal, role, content, source, created_at, platform_msg_id FROM session_messages \
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

        let mut out: Vec<(i64, DateTime<Utc>, baybo_model::ChatMessage)> = Vec::new();
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
            let source_str: String = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get source: {e}")))?;
            let created_us: i64 = row.get(4).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get created_at: {e}"))
            })?;
            let platform_msg_id: String = row.get(5).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get platform_msg_id: {e}"))
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
                rehydrate_message(&role, content, &source_str, platform_msg_id)?,
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
    ) -> Result<Vec<(i64, DateTime<Utc>, baybo_model::ChatMessage)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.pool.conn();
        // Forward difference: rows with ordinal strictly greater than the
        // client's cursor, capped at `limit`. The partial active index
        // bites the front of the range (`ordinal > N`) so a session
        // with hundreds of older rows pays nothing for them — only the
        // difference window is read.
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = conn
            .query(
                "SELECT ordinal, role, content, source, created_at, platform_msg_id FROM session_messages \
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

        let mut out: Vec<(i64, DateTime<Utc>, baybo_model::ChatMessage)> = Vec::new();
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
            let source_str: String = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get source: {e}")))?;
            let created_us: i64 = row.get(4).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get created_at: {e}"))
            })?;
            let platform_msg_id: String = row.get(5).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get platform_msg_id: {e}"))
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
                rehydrate_message(&role, content, &source_str, platform_msg_id)?,
            ));
        }
        Ok(out)
    }

    async fn find_message_ordinal_by_platform_msg_id(
        &self,
        session_id: &SessionId,
        platform_msg_id: &str,
    ) -> Result<Option<i64>> {
        if platform_msg_id.is_empty() {
            // Empty is the "no idempotency key" opt-out on the write
            // side; matching it would return an arbitrary keyless row.
            return Ok(None);
        }
        let conn = self.pool.conn();
        // No superseded filter: a compacted-away row still proves the
        // send was durably persisted, which is all the outbox needs.
        let mut rows = conn
            .query(
                "SELECT ordinal FROM session_messages \
                 WHERE session_id = ?1 AND platform_msg_id = ?2 \
                 ORDER BY ordinal DESC \
                 LIMIT 1",
                libsql::params![session_id.as_str().to_string(), platform_msg_id.to_string(),],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query platform_msg_id: {e}"))
            })?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        else {
            return Ok(None);
        };
        let ordinal: i64 = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get ord: {e}")))?;
        Ok(Some(ordinal))
    }

    async fn load_last_user_message(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(DateTime<Utc>, baybo_model::ChatMessage)>> {
        let conn = self.pool.conn();
        // Newest human-authored row (source `user` / `user_interjection`,
        // i.e. `from_user`). The partial active index makes `ORDER BY
        // ordinal DESC LIMIT 1` a single back-of-index probe regardless of
        // how many tool/agent rows the turn appended after the prompt.
        let mut rows = conn
            .query(
                "SELECT created_at, role, content, source, platform_msg_id FROM session_messages \
                 WHERE session_id = ?1 AND superseded_by IS NULL \
                   AND source IN ('user', 'user_interjection') \
                 ORDER BY ordinal DESC \
                 LIMIT 1",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql query last user message: {e}"))
            })?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        else {
            return Ok(None);
        };
        let created_us: i64 = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get created_at: {e}")))?;
        let created_at = super::time::from_us(created_us).ok_or_else(|| {
            StorageError::Storage(format!(
                "session_messages.created_at out of range: {created_us}"
            ))
        })?;
        let role: String = row
            .get(1)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get role: {e}")))?;
        let content_json: String = row
            .get(2)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get content: {e}")))?;
        let source_str: String = row
            .get(3)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get source: {e}")))?;
        let platform_msg_id: String = row.get(4).map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql get platform_msg_id: {e}"))
        })?;
        let content = serde_json::from_str(&content_json)
            .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
        Ok(Some((
            created_at,
            rehydrate_message(&role, content, &source_str, platform_msg_id)?,
        )))
    }

    async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredMessage>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT ordinal, superseded_by, role, content, created_at, source, platform_msg_id \
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
            let source_str: String = row
                .get(5)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get source: {e}")))?;
            let platform_msg_id: String = row.get(6).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get platform_msg_id: {e}"))
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
                message: rehydrate_message(&role, content, &source_str, platform_msg_id)?,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ChannelType, SessionState, TriggerSource, User};

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
            hidden: false,
            pinned: false,
            folder_id: None,
            title: None,
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

        store.delete(&s.id).await.unwrap();
        assert!(store.get(&s.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn control_events_round_trip_seq_kind_and_micros() {
        use baybo_model::ControlEventKind;

        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let s = make_root_session("ctl-1");
        store.save(&s).await.unwrap();

        // Sub-second precision down to the microsecond, to prove `created_at`
        // survives the µs-granular column round-trip exactly.
        let at = DateTime::from_timestamp_micros(1_700_000_000_123_456).expect("valid timestamp");

        // `seq` is assigned monotonically from 0, per session.
        let s0 = store
            .append_control_event(&s.id, -1, ControlEventKind::Command, "/stop", at)
            .await
            .unwrap();
        let s1 = store
            .append_control_event(&s.id, 7, ControlEventKind::NoticeInfo, "Stopped", at)
            .await
            .unwrap();
        let s2 = store
            .append_control_event(&s.id, 7, ControlEventKind::NoticeError, "boom", at)
            .await
            .unwrap();
        assert_eq!((s0, s1, s2), (0, 1, 2));

        let events = store.list_control_events(&s.id).await.unwrap();
        assert_eq!(events.len(), 3);

        // Ordered by seq; kind strings parse back to the typed enum; anchors,
        // text and the microsecond timestamp all preserved.
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[0].after_ordinal, -1);
        assert_eq!(events[0].kind, ControlEventKind::Command);
        assert_eq!(events[0].text, "/stop");
        assert_eq!(events[0].created_at, at);
        assert_eq!(events[1].kind, ControlEventKind::NoticeInfo);
        assert_eq!(events[1].after_ordinal, 7);
        assert_eq!(events[2].kind, ControlEventKind::NoticeError);
        assert_eq!(events[2].text, "boom");

        // A different session keeps its own independent seq space.
        let other = make_root_session("ctl-2");
        store.save(&other).await.unwrap();
        let o0 = store
            .append_control_event(&other.id, 0, ControlEventKind::NoticeWarn, "warn", at)
            .await
            .unwrap();
        assert_eq!(o0, 0, "seq is per-session, not global");
        assert_eq!(store.list_control_events(&s.id).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn list_all_skips_undeserializable_legacy_row() {
        // A row written by an older build can carry a `lineage.kind` this
        // build doesn't know (here `"system_maintenance"`). `list_all`
        // must skip that one row (log + continue) and still return every
        // good session, rather than erroring the whole listing and
        // 500-ing the CLI picker / web UI.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let good = make_root_session("good-1");
        store.save(&good).await.unwrap();

        // Hand-write such a row straight into the table — the current
        // `save` path can't construct one.
        let legacy_blob = r#"{
            "id": "maint-legacy",
            "user": {"id": "u1", "name": null, "channel": "tui"},
            "channel": "tui",
            "created_at": "2024-01-01T00:00:00Z",
            "last_active": "2024-01-01T00:00:00Z",
            "state": {},
            "root_session_id": "good-1",
            "trigger": {"kind": "system", "reason": "background_compression"},
            "lineage": {"parent_session_id": "good-1", "parent_job_id": "job-x", "kind": "system_maintenance"}
        }"#;
        assert!(
            serde_json::from_str::<Session>(legacy_blob).is_err(),
            "the unknown-lineage-kind blob must not deserialize"
        );
        store
            .pool
            .conn()
            .execute(
                "INSERT INTO sessions \
                 (id, root_session_id, trigger_kind, created_at, last_active, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![
                    "maint-legacy".to_string(),
                    "good-1".to_string(),
                    "system".to_string(),
                    super::super::time::to_us(Utc::now()),
                    super::super::time::to_us(Utc::now()),
                    legacy_blob.to_string(),
                ],
            )
            .await
            .unwrap();

        let listed = store.list_all().await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["good-1"],
            "row with unknown lineage kind is skipped"
        );
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
    async fn save_does_not_clobber_last_llm_set_by_set_last_llm() {
        // The exact race flagged in review: a concurrent full-blob `save`
        // — e.g. `touch` firing on the next inbound message, carrying a
        // stale in-memory `Session` with last_llm=None — must NOT wipe a
        // pin set via the targeted `set_last_llm`. Same flat-column guard
        // as `hidden`.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let s = make_root_session("pin-then-save");
        store.save(&s).await.unwrap(); // blob carries last_llm=None
        assert!(
            store
                .set_last_llm(&s.id, Some(&baybo_model::LlmEntryName::from("claude-opus")))
                .await
                .unwrap()
        );

        // Re-save the stale in-memory copy (still last_llm=None) — the
        // `touch` / background-actor persist path.
        store.save(&s).await.unwrap();

        let loaded = store.get(&s.id).await.unwrap().expect("row present");
        assert_eq!(
            loaded.state.last_llm,
            Some(baybo_model::LlmEntryName::from("claude-opus")),
            "save must preserve the last_llm column owned by set_last_llm"
        );

        // Clearing pins back to default-llm (NULL column → None).
        assert!(store.set_last_llm(&s.id, None).await.unwrap());
        let cleared = store.get(&s.id).await.unwrap().expect("row present");
        assert_eq!(cleared.state.last_llm, None);
    }

    #[tokio::test]
    async fn legacy_sessions_table_without_pinned_is_migrated() {
        // The "DB created before `pinned` existed" case the migration list
        // (libsql/mod.rs) handles. Simulate the pre-`pinned` schema by
        // dropping the column the fresh `init_db` created, write a row the
        // way an old build would (no `pinned`), then re-run `init_db` — the
        // boot path a new binary takes. Without the ALTER migration the
        // store's `SELECT … pinned` would fail with "no such column"; with
        // it the column comes back and the old row defaults to unpinned.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        pool.conn()
            .execute("ALTER TABLE sessions DROP COLUMN pinned", libsql::params![])
            .await
            .unwrap();
        let data = serde_json::to_string(&make_root_session("legacy-1")).unwrap();
        pool.conn()
            .execute(
                "INSERT INTO sessions \
                 (id, root_session_id, trigger_kind, created_at, last_active, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![
                    "legacy-1".to_string(),
                    "legacy-1".to_string(),
                    "user".to_string(),
                    super::super::time::to_us(Utc::now()),
                    super::super::time::to_us(Utc::now()),
                    data,
                ],
            )
            .await
            .unwrap();
        // Re-running init_db applies the idempotent ALTER (re-adds pinned).
        pool.init_db().await.unwrap();

        let store = LibsqlSessionStore::new(pool);
        let id = SessionId::from("legacy-1");
        let loaded = store.get(&id).await.unwrap().expect("legacy row present");
        assert!(!loaded.pinned, "migrated legacy row defaults to unpinned");
        // And the column is now writable like any other.
        assert!(store.set_pinned(&id, true).await.unwrap());
        assert!(store.get(&id).await.unwrap().unwrap().pinned);
    }

    #[tokio::test]
    async fn message_platform_msg_id_round_trips_and_legacy_defaults_empty() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool.clone());
        let session = make_root_session("platform-msg-id");
        store.save(&session).await.unwrap();

        let msg = baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(
            "from device".into(),
        )])
        .with_platform_msg_id("device-msg-1");
        store
            .append_session_message(&session.id, &msg)
            .await
            .unwrap();
        let loaded = store
            .load_active_session_messages(&session.id)
            .await
            .unwrap();
        assert_eq!(loaded[0].platform_msg_id(), "device-msg-1");

        pool.conn()
            .execute(
                "ALTER TABLE session_messages DROP COLUMN platform_msg_id",
                libsql::params![],
            )
            .await
            .unwrap();
        pool.conn()
            .execute(
                "INSERT INTO session_messages \
                 (session_id, ordinal, role, content, created_at, source) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![
                    "platform-msg-id".to_string(),
                    1_i64,
                    "user".to_string(),
                    serde_json::to_string(&vec![baybo_model::ContentBlock::Text("legacy".into())])
                        .unwrap(),
                    super::super::time::to_us(Utc::now()),
                    "user".to_string(),
                ],
            )
            .await
            .unwrap();
        pool.init_db().await.unwrap();

        let store = LibsqlSessionStore::new(pool);
        let loaded = store
            .load_active_session_messages(&session.id)
            .await
            .unwrap();
        assert_eq!(loaded[0].platform_msg_id(), "");
        assert_eq!(loaded[1].platform_msg_id(), "");
    }

    #[tokio::test]
    async fn save_does_not_clobber_pinned_set_by_set_pinned() {
        // Same race the `hidden` / `last_llm` guards defend against: a
        // concurrent full-blob `save` (a `touch` on the next inbound
        // message, carrying a stale in-memory `Session` with pinned=false)
        // must NOT wipe a pin set via the targeted `set_pinned`.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let s = make_root_session("pin-then-save");
        store.save(&s).await.unwrap();
        assert!(store.set_pinned(&s.id, true).await.unwrap());

        // Re-save the stale in-memory copy (still pinned=false).
        store.save(&s).await.unwrap();

        let loaded = store.get(&s.id).await.unwrap().expect("row present");
        assert!(
            loaded.pinned,
            "save must preserve the pinned column owned by set_pinned"
        );

        // Unpin clears it back.
        assert!(store.set_pinned(&s.id, false).await.unwrap());
        let cleared = store.get(&s.id).await.unwrap().expect("row present");
        assert!(!cleared.pinned);
    }

    #[tokio::test]
    async fn list_by_channel_reflects_pinned_column() {
        // `list_by_channel` / `list_all` must project the flat `pinned`
        // column the same way `get` does, so a listed `Session` carries
        // the authoritative flag rather than the (stale) blob value.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let mut s = make_root_session("pin-list");
        s.channel = ChannelType::http();
        store.save(&s).await.unwrap();
        assert!(store.set_pinned(&s.id, true).await.unwrap());

        let listed = store.list_by_channel(&ChannelType::http()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].pinned,
            "pinned must reflect the column in list projections"
        );
    }

    #[tokio::test]
    async fn list_by_channel_reflects_last_llm_column() {
        // `list_by_channel` / `list_all` must project the flat `last_llm`
        // column the same way `get` does, so a listed `Session` carries
        // the authoritative pin rather than the (stale) blob value.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let mut s = make_root_session("pin-list");
        s.channel = ChannelType::http();
        store.save(&s).await.unwrap();
        assert!(
            store
                .set_last_llm(&s.id, Some(&baybo_model::LlmEntryName::from("gpt-4o")))
                .await
                .unwrap()
        );

        let listed = store.list_by_channel(&ChannelType::http()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].state.last_llm,
            Some(baybo_model::LlmEntryName::from("gpt-4o")),
            "last_llm must reflect the column in list projections"
        );
    }

    #[tokio::test]
    async fn set_folder_round_trips_and_clears() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let s = make_root_session("fld-1");
        store.save(&s).await.unwrap();

        let fid = baybo_model::FolderId::from("folder-x");
        assert!(store.set_folder(&s.id, Some(&fid)).await.unwrap());
        assert_eq!(
            store.get(&s.id).await.unwrap().unwrap().folder_id,
            Some(fid.clone())
        );

        // None clears back to uncategorized.
        assert!(store.set_folder(&s.id, None).await.unwrap());
        assert_eq!(store.get(&s.id).await.unwrap().unwrap().folder_id, None);

        // Unknown session id reports no row updated.
        assert!(
            !store
                .set_folder(&SessionId::from("nope"), Some(&fid))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn save_does_not_clobber_folder_id_set_by_set_folder() {
        // Same race the `pinned` / `hidden` guards defend against: a
        // concurrent full-blob `save` carrying a stale in-memory `Session`
        // (folder_id = None) must NOT wipe an assignment set via the
        // targeted `set_folder`.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let s = make_root_session("fld-then-save");
        store.save(&s).await.unwrap();
        let fid = baybo_model::FolderId::from("keepme");
        assert!(store.set_folder(&s.id, Some(&fid)).await.unwrap());

        // Re-save the stale in-memory copy (still folder_id = None).
        store.save(&s).await.unwrap();

        assert_eq!(
            store.get(&s.id).await.unwrap().unwrap().folder_id,
            Some(fid),
            "save must preserve the folder_id column owned by set_folder"
        );
    }

    #[tokio::test]
    async fn list_by_channel_reflects_folder_id_column() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let mut s = make_root_session("fld-list");
        s.channel = ChannelType::http();
        store.save(&s).await.unwrap();
        let fid = baybo_model::FolderId::from("folder-list");
        assert!(store.set_folder(&s.id, Some(&fid)).await.unwrap());

        let listed = store.list_by_channel(&ChannelType::http()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].folder_id,
            Some(fid),
            "folder_id must reflect the column in list projections (guards the index renumber)"
        );
    }

    #[tokio::test]
    async fn legacy_sessions_table_without_folder_id_is_migrated() {
        // The "DB created before `folder_id` existed" case. Drop the column
        // the fresh `init_db` created, write a row the old way, then re-run
        // `init_db` (the boot path) — the idempotent ALTER re-adds it and
        // the old row defaults to uncategorized.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        // SQLite refuses to drop an indexed column, so drop the index first
        // — this also recreates the genuine pre-folder_id schema (no column,
        // no index), the exact state `init_db`'s migration must recover from.
        pool.conn()
            .execute(
                "DROP INDEX IF EXISTS idx_sessions_folder",
                libsql::params![],
            )
            .await
            .unwrap();
        pool.conn()
            .execute(
                "ALTER TABLE sessions DROP COLUMN folder_id",
                libsql::params![],
            )
            .await
            .unwrap();
        let data = serde_json::to_string(&make_root_session("legacy-fld")).unwrap();
        pool.conn()
            .execute(
                "INSERT INTO sessions \
                 (id, root_session_id, trigger_kind, created_at, last_active, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![
                    "legacy-fld".to_string(),
                    "legacy-fld".to_string(),
                    "user".to_string(),
                    super::super::time::to_us(Utc::now()),
                    super::super::time::to_us(Utc::now()),
                    data,
                ],
            )
            .await
            .unwrap();
        pool.init_db().await.unwrap();

        let store = LibsqlSessionStore::new(pool);
        let id = SessionId::from("legacy-fld");
        let loaded = store.get(&id).await.unwrap().expect("legacy row present");
        assert_eq!(
            loaded.folder_id, None,
            "migrated legacy row defaults to uncategorized"
        );
        let fid = baybo_model::FolderId::from("now-filed");
        assert!(store.set_folder(&id, Some(&fid)).await.unwrap());
        assert_eq!(store.get(&id).await.unwrap().unwrap().folder_id, Some(fid));
    }

    #[tokio::test]
    async fn set_title_round_trips_and_clears() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let s = make_root_session("title-1");
        store.save(&s).await.unwrap();

        assert!(
            store
                .set_title(&s.id, Some("Fix login redirect"))
                .await
                .unwrap()
        );
        assert_eq!(
            store.get(&s.id).await.unwrap().unwrap().title.as_deref(),
            Some("Fix login redirect")
        );

        assert!(store.set_title(&s.id, None).await.unwrap());
        assert_eq!(store.get(&s.id).await.unwrap().unwrap().title, None);

        assert!(
            !store
                .set_title(&SessionId::from("nope"), Some("x"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn save_does_not_clobber_title_set_by_set_title() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let s = make_root_session("title-then-save");
        store.save(&s).await.unwrap();
        assert!(store.set_title(&s.id, Some("Keep me")).await.unwrap());

        store.save(&s).await.unwrap();

        assert_eq!(
            store.get(&s.id).await.unwrap().unwrap().title.as_deref(),
            Some("Keep me"),
            "save must preserve the title column owned by set_title"
        );
    }

    #[tokio::test]
    async fn list_by_channel_reflects_title_column() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let mut s = make_root_session("title-list");
        s.channel = ChannelType::http();
        store.save(&s).await.unwrap();
        assert!(store.set_title(&s.id, Some("Listed title")).await.unwrap());

        let listed = store.list_by_channel(&ChannelType::http()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].title.as_deref(),
            Some("Listed title"),
            "title must reflect the column in list projections (guards the index renumber)"
        );
    }

    #[tokio::test]
    async fn legacy_sessions_table_without_title_is_migrated() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        pool.conn()
            .execute("ALTER TABLE sessions DROP COLUMN title", libsql::params![])
            .await
            .unwrap();
        let data = serde_json::to_string(&make_root_session("legacy-title")).unwrap();
        pool.conn()
            .execute(
                "INSERT INTO sessions \
                 (id, root_session_id, trigger_kind, created_at, last_active, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![
                    "legacy-title".to_string(),
                    "legacy-title".to_string(),
                    "user".to_string(),
                    super::super::time::to_us(Utc::now()),
                    super::super::time::to_us(Utc::now()),
                    data,
                ],
            )
            .await
            .unwrap();
        pool.init_db().await.unwrap();

        let store = LibsqlSessionStore::new(pool);
        let id = SessionId::from("legacy-title");
        let loaded = store.get(&id).await.unwrap().expect("legacy row present");
        assert_eq!(
            loaded.title, None,
            "migrated legacy row defaults to no title"
        );
        assert!(store.set_title(&id, Some("Now titled")).await.unwrap());
        assert_eq!(
            store.get(&id).await.unwrap().unwrap().title.as_deref(),
            Some("Now titled")
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
            let msg = baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(
                format!("msg-{i}"),
            )]);
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
    async fn load_last_user_message_finds_freshest_human_turn_past_tool_churn() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let session = make_root_session("preview-me");
        store.save(&session).await.unwrap();
        let text = |s: &str| vec![baybo_model::ContentBlock::Text(s.to_owned())];

        // No user turn yet -> None.
        assert!(
            store
                .load_last_user_message(&session.id)
                .await
                .unwrap()
                .is_none()
        );

        store
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::user(text("first prompt")),
            )
            .await
            .unwrap();
        store
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::assistant(text("first reply")),
            )
            .await
            .unwrap();
        store
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::user(text("the freshest prompt")),
            )
            .await
            .unwrap();
        // A long tool loop after the prompt: agent-sourced rows that must
        // not shadow the user's turn (the bug the targeted query fixes).
        for i in 0..12 {
            store
                .append_session_message(
                    &session.id,
                    &baybo_model::ChatMessage::assistant(text(&format!("tool churn {i}"))),
                )
                .await
                .unwrap();
        }

        let (_, msg) = store
            .load_last_user_message(&session.id)
            .await
            .unwrap()
            .expect("a user turn");
        assert!(msg.from_user());
        assert_eq!(
            msg.content,
            text("the freshest prompt"),
            "returns the newest human-authored turn, not the trailing agent rows"
        );
    }

    #[tokio::test]
    async fn load_last_user_message_counts_interjections_not_agent_user_rows() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let session = make_root_session("interjection-me");
        store.save(&session).await.unwrap();
        let text = |s: &str| vec![baybo_model::ContentBlock::Text(s.to_owned())];

        store
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::user(text("genuine")),
            )
            .await
            .unwrap();
        store
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::user_interjection(text("interjected")),
            )
            .await
            .unwrap();
        // Agent-injected Role::User row (e.g. a skill reminder) must not count.
        store
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::agent_context(text("injected reminder")),
            )
            .await
            .unwrap();

        let (_, msg) = store
            .load_last_user_message(&session.id)
            .await
            .unwrap()
            .expect("a user turn");
        assert_eq!(
            msg.content,
            text("interjected"),
            "interjection is human-authored; agent_context is not"
        );
    }

    #[tokio::test]
    async fn load_active_session_messages_since_forward_pages_above_cursor() {
        // Same 7-row fixture as the `_tail` test, but exercising the
        // forward difference scan the REST sync endpoint uses to
        // deliver missed rows to a client presenting its cursor.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);
        let session = make_root_session("catch-up-me");
        store.save(&session).await.unwrap();
        for i in 0..7 {
            let msg = baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(
                format!("msg-{i}"),
            )]);
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
        let ords: Vec<i64> = after_3.iter().map(|(o, _, _)| *o).collect();
        assert_eq!(ords, vec![4, 5, 6]);

        // `limit` is the cap: from cursor 0, ask for 2 — the first
        // two missed rows (1, 2), not the whole tail.
        let cap = store
            .load_active_session_messages_since(&session.id, 0, 2)
            .await
            .unwrap();
        let cap_ords: Vec<i64> = cap.iter().map(|(o, _, _)| *o).collect();
        assert_eq!(cap_ords, vec![1, 2]);

        // Caught up: the cursor is at (or past) the latest row, so
        // the slice is empty. `limit + 1` over-fetch sees zero ⇒
        // server's "nothing missed" branch.
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
        let all_ords: Vec<i64> = all.iter().map(|(o, _, _)| *o).collect();
        assert_eq!(all_ords, vec![0, 1, 2, 3, 4, 5, 6]);
    }
}
