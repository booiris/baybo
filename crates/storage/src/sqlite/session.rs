use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_model::{
    AgentFramework, AgentProfileId, ChatMessage, ControlEvent, ControlEventKind, FolderId,
    LineageKind, LlmEntryName, Session, SessionId,
};
use baybo_store::StorageError;
use baybo_store::session::{Result, SessionMessageAppendOutcome, SessionStore, StoredMessage};

pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// SQL string for the `lineage_kind` column, matched by both
/// `lineage_kind_str` (write side) and `list_lineage_children`
/// (read side). Keeping it here as a named constant prevents drift
/// when a new variant lands.
pub(super) const LINEAGE_KIND_SUBAGENT: &str = "subagent";

/// Raw `session_messages` columns as they come off a row, decoded into a
/// [`ChatMessage`] only once the connection has been released: `(role,
/// content_json, source, platform_msg_id)`. The serde/enum decode can fail with
/// a non-`Internal` [`StorageError`], which the pool's `anyhow` closure cannot
/// build, so every read path collects these tuples first and rehydrates after.
type RawMessageRow = (String, String, String, String);

/// [`RawMessageRow`] plus the row's `ordinal` and `created_at` (Unix µs), for
/// the paging reads that surface both.
type RawMessageRowWithMeta = (i64, String, String, String, i64, String);

/// Raw `sessions` columns projected by the list reads:
/// `(data, hidden, last_llm, pinned, id, folder_id, archived, title,
/// agent_id, agent_framework)`.
type RawSessionListRow = (
    String,
    i64,
    Option<String>,
    i64,
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn lineage_kind_str(s: &Session) -> Option<&'static str> {
    s.lineage.as_ref().map(|l| match l.kind {
        LineageKind::Subagent => LINEAGE_KIND_SUBAGENT,
    })
}

/// Parse the `agent_framework` column into a typed value. `None` (column
/// unset — an unbound session) passes through as `None`; a stored tag this
/// build doesn't recognise (e.g. written by a newer build) is a hard error —
/// callers decide whether that fails the whole read (`get`) or skips just
/// this row (`list_all` / `list_by_channel`).
fn parse_agent_framework_col(col: Option<String>) -> Result<Option<AgentFramework>> {
    match col {
        None => Ok(None),
        Some(s) => AgentFramework::parse(&s)
            .map(Some)
            .ok_or_else(|| StorageError::Storage(format!("unknown agent_framework {s:?}"))),
    }
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
        MessageSource::CronNotification => ChatMessage::cron_notification(content),
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

/// Decode one [`RawMessageRow`] into a [`ChatMessage`].
fn decode_message_row(row: RawMessageRow) -> Result<ChatMessage> {
    let (role, content_json, source_str, platform_msg_id) = row;
    let content = serde_json::from_str(&content_json)
        .map_err(|e| StorageError::Storage(format!("deserialize message content: {e}")))?;
    rehydrate_message(&role, content, &source_str, platform_msg_id)
}

/// Rebuild a [`Session`] from a [`RawSessionListRow`], patching the flat
/// columns over the JSON blob. Flat columns are authoritative; targeted setters
/// leave the JSON blob untouched to avoid load/save races. An unrecognised
/// `agent_framework` tag is a hard error (like a blob that fails to
/// deserialize) — the list paths skip-and-warn on any `Err` here.
fn decode_session_row(row: &RawSessionListRow) -> Result<Session> {
    let (
        data,
        hidden_col,
        last_llm_col,
        pinned_col,
        _id,
        folder_id_col,
        archived_col,
        title_col,
        agent_id_col,
        agent_framework_col,
    ) = row;
    let mut session: Session = serde_json::from_str(data)
        .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
    session.hidden = *hidden_col != 0;
    session.state.last_llm = last_llm_col.clone().map(LlmEntryName::from);
    session.pinned = *pinned_col != 0;
    session.folder_id = folder_id_col.clone().map(FolderId::from);
    session.archived = *archived_col != 0;
    session.title = title_col.clone();
    session.state.agent_id = agent_id_col.clone().map(AgentProfileId::from);
    session.state.agent_framework = parse_agent_framework_col(agent_framework_col.clone())?;
    Ok(session)
}

fn read_session_list_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSessionListRow> {
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
    ))
}

fn read_message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawMessageRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Session>> {
        let sid = session_id.as_str().to_string();
        let row = self
            .pool
            .interact("sessions.get", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT data, hidden, last_llm, pinned, folder_id, archived, title, \
                         agent_id, agent_framework FROM sessions WHERE id = ?1",
                        rusqlite::params![sid],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, Option<String>>(4)?,
                                row.get::<_, i64>(5)?,
                                row.get::<_, Option<String>>(6)?,
                                row.get::<_, Option<String>>(7)?,
                                row.get::<_, Option<String>>(8)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;

        let Some((
            data,
            hidden_col,
            last_llm_col,
            pinned_col,
            folder_id_col,
            archived_col,
            title_col,
            agent_id_col,
            agent_framework_col,
        )) = row
        else {
            return Ok(None);
        };
        let mut session: Session = serde_json::from_str(&data)
            .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
        // Flat columns are authoritative; targeted setters leave the
        // JSON blob untouched to avoid load/save races.
        session.hidden = hidden_col != 0;
        session.state.last_llm = last_llm_col.map(LlmEntryName::from);
        session.pinned = pinned_col != 0;
        session.folder_id = folder_id_col.map(FolderId::from);
        session.archived = archived_col != 0;
        session.title = title_col;
        session.state.agent_id = agent_id_col.map(AgentProfileId::from);
        // Unlike the list paths (which skip-and-warn a corrupt row), a
        // targeted `get` surfaces an unrecognised framework as a hard error.
        session.state.agent_framework = parse_agent_framework_col(agent_framework_col)?;
        Ok(Some(session))
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let data = serde_json::to_string(session)
            .map_err(|e| StorageError::Storage(format!("serialize session: {e}")))?;
        let trigger_kind = match session.trigger.kind() {
            baybo_model::TriggerKind::User => "user",
            baybo_model::TriggerKind::Cron => "cron",
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
        let archived_flag: i64 = if session.archived { 1 } else { 0 };
        let agent_id = session
            .state
            .agent_id
            .as_ref()
            .map(|a| a.as_str().to_string());
        let agent_framework = session.state.agent_framework.map(|f| f.as_str().to_owned());
        let id = session.id.as_str().to_string();
        let root_id = session.root_session_id.as_str().to_string();
        let trigger_kind = trigger_kind.to_string();
        let created_us = super::time::to_us(session.created_at);
        let last_active_us = super::time::to_us(session.last_active);
        // Upsert in place (NOT `INSERT OR REPLACE`, which delete+reinserts
        // the row). The DO UPDATE clause omits the flat columns owned by
        // targeted setters (`hidden`, `last_llm`, `pinned`, `folder_id`,
        // `archived`, `title`, `read_cursor`) so a stale in-memory `Session`
        // (e.g. a background-subagent persist after the user hid, pinned or
        // archived the conversation) cannot clobber them. `hidden` / `pinned` /
        // `archived` are seeded only on a brand-new row (`?10`–`?12`); `get`
        // reads all flat columns as authoritative over the JSON blob.
        // `agent_id` / `agent_framework` (`?13`–`?14`) have no setter at all —
        // seeded only on a brand-new row and never written again, so the agent
        // binding is structurally immutable rather than merely guard-enforced.
        self.pool
            .interact("sessions.save", move |conn| {
                conn.execute(
                    "INSERT INTO sessions \
                     (id, root_session_id, trigger_kind, parent_session_id, parent_job_id, \
                      parent_span_id, lineage_kind, created_at, last_active, \
                      hidden, pinned, archived, agent_id, agent_framework, data) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
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
                    rusqlite::params![
                        id,
                        root_id,
                        trigger_kind,
                        parent_session,
                        parent_job,
                        parent_span,
                        lineage_kind,
                        created_us,
                        last_active_us,
                        hidden_flag,
                        pinned_flag,
                        archived_flag,
                        agent_id,
                        agent_framework,
                        data,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn set_hidden(&self, session_id: &SessionId, hidden: bool) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        let flag: i64 = if hidden { 1 } else { 0 };
        // Targeted UPDATE on the flat column only — the JSON `data`
        // blob is left alone so a concurrent `touch` (which goes
        // through load + save) can't lose this write. `get` patches
        // `Session.hidden` from the column on read, so observers see
        // the up-to-date value regardless of blob staleness.
        let affected = self
            .pool
            .interact("sessions.set_hidden", move |conn| {
                Ok(conn.execute(
                    "UPDATE sessions SET hidden = ?2 WHERE id = ?1",
                    rusqlite::params![sid, flag],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_last_llm(
        &self,
        session_id: &SessionId,
        llm: Option<&LlmEntryName>,
    ) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        // Targeted UPDATE on the flat column only — like `set_hidden`,
        // the JSON `data` blob is left alone so a concurrent `touch`
        // (load + full save) can't lose this write. `get` patches
        // `Session.state.last_llm` from the column on read. `NULL`
        // clears the pin back to `default-llm`.
        let value: Option<String> = llm.map(|n| n.as_str().to_string());
        let affected = self
            .pool
            .interact("sessions.set_last_llm", move |conn| {
                Ok(conn.execute(
                    "UPDATE sessions SET last_llm = ?2 WHERE id = ?1",
                    rusqlite::params![sid, value],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_pinned(&self, session_id: &SessionId, pinned: bool) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        let flag: i64 = if pinned { 1 } else { 0 };
        // Targeted UPDATE on the flat column only — like `set_hidden`,
        // the JSON `data` blob is left alone so a concurrent `touch`
        // (load + full save) can't lose this write. `get` patches
        // `Session.pinned` from the column on read.
        let affected = self
            .pool
            .interact("sessions.set_pinned", move |conn| {
                Ok(conn.execute(
                    "UPDATE sessions SET pinned = ?2 WHERE id = ?1",
                    rusqlite::params![sid, flag],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_archived(&self, session_id: &SessionId, archived: bool) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        let flag: i64 = if archived { 1 } else { 0 };
        // Targeted UPDATE on the flat column only — like `set_hidden`,
        // the JSON `data` blob is left alone so a concurrent `touch`
        // (load + full save) can't lose this write. `get` patches
        // `Session.archived` from the column on read.
        let affected = self
            .pool
            .interact("sessions.set_archived", move |conn| {
                Ok(conn.execute(
                    "UPDATE sessions SET archived = ?2 WHERE id = ?1",
                    rusqlite::params![sid, flag],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_folder(
        &self,
        session_id: &SessionId,
        folder_id: Option<&FolderId>,
    ) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        // Targeted UPDATE on the flat column only — like `set_hidden` /
        // `set_pinned`, the JSON `data` blob is left alone so a concurrent
        // `touch` (load + full save) can't lose this write. `get` patches
        // `Session.folder_id` from the column on read. `NULL` clears the
        // assignment back to uncategorized.
        let value: Option<String> = folder_id.map(|f| f.as_str().to_string());
        let affected = self
            .pool
            .interact("sessions.set_folder", move |conn| {
                Ok(conn.execute(
                    "UPDATE sessions SET folder_id = ?2 WHERE id = ?1",
                    rusqlite::params![sid, value],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn set_read_cursor(&self, session_id: &SessionId, ordinal: i64) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        // Targeted, max-wins UPDATE on the flat column only — the `CASE`
        // guards against a reordered/stale marker regressing the cursor (a
        // background tab PUTting an older read position must not undo a newer
        // one). The JSON `data` blob is untouched, like `set_pinned`.
        let affected = self
            .pool
            .interact("sessions.set_read_cursor", move |conn| {
                Ok(conn.execute(
                    "UPDATE sessions \
                     SET read_cursor = CASE \
                         WHEN read_cursor IS NULL OR ?2 > read_cursor THEN ?2 \
                         ELSE read_cursor END \
                     WHERE id = ?1",
                    rusqlite::params![sid, ordinal],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn read_cursor(&self, session_id: &SessionId) -> Result<Option<i64>> {
        let sid = session_id.as_str().to_string();
        self.pool
            .interact("sessions.read_cursor", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT read_cursor FROM sessions WHERE id = ?1",
                        rusqlite::params![sid],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten())
            })
            .await
    }

    async fn set_title(&self, session_id: &SessionId, title: Option<&str>) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        let value: Option<String> = title.map(|t| t.to_string());
        let affected = self
            .pool
            .interact("sessions.set_title", move |conn| {
                Ok(conn.execute(
                    "UPDATE sessions SET title = ?2 WHERE id = ?1",
                    rusqlite::params![sid, value],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        let sid = session_id.as_str().to_string();
        self.pool
            .interact("sessions.delete", move |conn| {
                // The message-log cascade and the session-row delete must commit
                // as a unit (see below); BEGIN IMMEDIATE takes the write lock up
                // front so the pair runs without an interleaved writer.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // Cascade the message log first — there's no FK in sqlite, so
                // a stranded `session_messages` row would otherwise outlive
                // its parent.
                tx.execute(
                    "DELETE FROM session_messages WHERE session_id = ?1",
                    rusqlite::params![sid],
                )?;

                let affected =
                    tx.execute("DELETE FROM sessions WHERE id = ?1", rusqlite::params![sid])?;
                if affected == 0 {
                    // Dropping the transaction rolls it back.
                    drop(tx);
                    return Ok(false);
                }
                tx.commit()?;
                Ok(true)
            })
            .await
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<SessionId>> {
        let before_us = super::time::to_us(before);
        let ids = self
            .pool
            .interact("sessions.list_expired", move |conn| {
                let mut stmt = conn.prepare("SELECT id FROM sessions WHERE last_active < ?1")?;
                let ids = stmt
                    .query_map(rusqlite::params![before_us], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(ids)
            })
            .await?;
        Ok(ids.into_iter().map(SessionId::from).collect())
    }

    async fn list_all(&self) -> Result<Vec<Session>> {
        // Project the flat `hidden` column — `set_hidden` writes there
        // directly without rewriting the JSON `data` blob, so trusting
        // only the blob would read stale values. `id` rides along purely
        // so a row whose `data` blob fails to deserialize (e.g. one
        // written by an older build whose lineage kind this build doesn't
        // know) can be named in the skip warning.
        let rows = self
            .pool
            .interact("sessions.list_all", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT data, hidden, last_llm, pinned, id, folder_id, archived, title, \
                     agent_id, agent_framework FROM sessions \
                     ORDER BY last_active DESC",
                )?;
                let rows = stmt
                    .query_map([], read_session_list_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut sessions = Vec::new();
        for row in &rows {
            // A single undeserializable row (e.g. one written by an older
            // build whose `lineage.kind` this build doesn't know) must
            // degrade to "silently absent from the listing", never fail
            // the whole listing and 500 the CLI picker / web UI.
            match decode_session_row(row) {
                Ok(session) => sessions.push(session),
                Err(e) => tracing::warn!(
                    session_id = %row.4,
                    "skipping session row that failed to deserialize: {e}"
                ),
            }
        }
        Ok(sessions)
    }

    async fn list_by_channel(&self, channel: &baybo_model::ChannelType) -> Result<Vec<Session>> {
        // Push the channel filter into SQL via `json_extract` — the
        // sessions table doesn't carry `channel` as a flat column
        // (it rides inside the JSON `data` blob), so a real index
        // isn't available without a schema migration. This is still
        // a full table scan, but non-matching rows never ship their
        // `data` blob out of sqlite or pay the serde decode
        // in userland, which is the cost we actually care about for
        // a long-running gateway with thousands of bot sessions.
        let channel = channel.as_str().to_string();
        let rows = self
            .pool
            .interact("sessions.list_by_channel", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT data, hidden, last_llm, pinned, id, folder_id, archived, title, \
                     agent_id, agent_framework FROM sessions \
                     WHERE json_extract(data, '$.channel') = ?1 \
                     ORDER BY last_active DESC",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![channel], read_session_list_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut sessions = Vec::new();
        for row in &rows {
            // Same skip-on-error discipline as `list_all`: a row whose
            // blob fails to deserialize drops out of the listing rather
            // than failing the whole chat-list query.
            match decode_session_row(row) {
                Ok(session) => sessions.push(session),
                Err(e) => tracing::warn!(
                    session_id = %row.4,
                    "skipping session row that failed to deserialize: {e}"
                ),
            }
        }
        Ok(sessions)
    }

    async fn list_lineage_children(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<(SessionId, LineageKind)>> {
        let parent = parent_session_id.as_str().to_string();
        self.pool
            .interact("sessions.list_lineage_children", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, lineage_kind FROM sessions \
                     WHERE parent_session_id = ?1 AND lineage_kind IS NOT NULL",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![parent], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                let mut children = Vec::new();
                for (id, kind_tag) in rows {
                    // The variant is payload-free, so the kind tag alone
                    // reconstructs the `LineageKind` — no JSON decode needed.
                    // An unrecognised tag is skipped rather than erroring the whole
                    // listing.
                    let kind = match kind_tag.as_str() {
                        LINEAGE_KIND_SUBAGENT => LineageKind::Subagent,
                        _ => continue,
                    };
                    children.push((SessionId::from(id), kind));
                }
                Ok(children)
            })
            .await
    }

    async fn append_session_message(
        &self,
        session_id: &SessionId,
        message: &ChatMessage,
    ) -> Result<i64> {
        let sid = session_id.as_str().to_string();
        let role = message.role.as_str().to_string();
        let content = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::Storage(format!("serialize message content: {e}")))?;
        let now_us = super::time::to_us(chrono::Utc::now());
        let source = message.source().as_str().to_string();
        let platform_msg_id = message.platform_msg_id().to_string();
        // `INSERT … SELECT COALESCE(MAX(ordinal),-1)+1 … RETURNING` keeps
        // ordinals contiguous without an explicit sequence and hands
        // back the assigned value in one round trip. The actor model
        // serialises writes per session, so there's no concurrent-
        // append race to defend against here.
        self.pool
            .interact("sessions.append_session_message", move |conn| {
                let ordinal: i64 = conn
                    .query_row(
                        "INSERT INTO session_messages \
                     (session_id, ordinal, role, content, created_at, source, platform_msg_id) \
                     SELECT ?1, COALESCE(MAX(ordinal), -1) + 1, ?2, ?3, ?4, ?5, ?6 \
                     FROM session_messages WHERE session_id = ?1 \
                     RETURNING ordinal",
                        rusqlite::params![sid, role, content, now_us, source, platform_msg_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        anyhow::anyhow!("INSERT … RETURNING returned no rows for session_messages")
                    })?;
                Ok(ordinal)
            })
            .await
    }

    async fn append_session_message_idempotent(
        &self,
        session_id: &SessionId,
        source_event_id: &str,
        message: &ChatMessage,
    ) -> Result<SessionMessageAppendOutcome> {
        if source_event_id.is_empty() {
            return Err(StorageError::Storage(
                "source_event_id must not be empty".to_string(),
            ));
        }

        let role = message.role.as_str().to_string();
        let content = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::Storage(format!("serialize message content: {e}")))?;
        let now_us = super::time::to_us(chrono::Utc::now());
        let session_id = session_id.as_str().to_string();
        let source_event_id = source_event_id.to_string();
        let source = message.source().as_str().to_string();
        let platform_msg_id = message.platform_msg_id().to_string();
        self.pool
            .interact("sessions.append_session_message_idempotent", move |conn| {
                let inserted: Option<i64> = conn
                    .query_row(
                        "INSERT INTO session_messages \
                         (session_id, ordinal, role, content, created_at, source, \
                          platform_msg_id, source_event_id) \
                         SELECT ?1, COALESCE(MAX(ordinal), -1) + 1, ?2, ?3, ?4, ?5, ?6, ?7 \
                         FROM session_messages WHERE session_id = ?1 \
                         ON CONFLICT DO NOTHING \
                         RETURNING ordinal",
                        rusqlite::params![
                            session_id,
                            role,
                            content,
                            now_us,
                            source,
                            platform_msg_id,
                            source_event_id,
                        ],
                        |row| row.get(0),
                    )
                    .optional()?;

                if let Some(ordinal) = inserted {
                    return Ok(SessionMessageAppendOutcome::Inserted { ordinal });
                }

                let ordinal: i64 = conn
                    .query_row(
                        "SELECT ordinal FROM session_messages \
                         WHERE session_id = ?1 AND source_event_id = ?2",
                        rusqlite::params![session_id, source_event_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "idempotent session_message insert returned no row and no existing key"
                        )
                    })?;
                Ok(SessionMessageAppendOutcome::Existing { ordinal })
            })
            .await
    }

    async fn find_message_ordinal_by_source_event_id(
        &self,
        session_id: &SessionId,
        source_event_id: &str,
    ) -> Result<Option<i64>> {
        let sid = session_id.as_str().to_string();
        let source_event_id = source_event_id.to_string();
        self.pool
            .interact(
                "sessions.find_message_ordinal_by_source_event_id",
                move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT ordinal FROM session_messages \
                             WHERE session_id = ?1 AND source_event_id = ?2",
                            rusqlite::params![sid, source_event_id],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()?)
                },
            )
            .await
    }

    async fn append_control_event(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        created_at: DateTime<Utc>,
    ) -> Result<i64> {
        let sid = session_id.as_str().to_string();
        let kind = kind.as_str().to_string();
        let text = text.to_string();
        let created_us = super::time::to_us(created_at);
        self.pool
            .interact("sessions.append_control_event", move |conn| {
                let seq: i64 = conn
                    .query_row(
                        "INSERT INTO session_control_events \
                     (session_id, seq, after_ordinal, kind, text, created_at) \
                     SELECT ?1, COALESCE(MAX(seq), -1) + 1, ?2, ?3, ?4, ?5 \
                     FROM session_control_events WHERE session_id = ?1 \
                     RETURNING seq",
                        rusqlite::params![sid, after_ordinal, kind, text, created_us],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "INSERT … RETURNING returned no rows for session_control_events"
                        )
                    })?;
                Ok(seq)
            })
            .await
    }

    async fn list_control_events(&self, session_id: &SessionId) -> Result<Vec<ControlEvent>> {
        let sid = session_id.as_str().to_string();
        let rows = self
            .pool
            .interact("sessions.list_control_events", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT seq, after_ordinal, kind, text, created_at FROM session_control_events \
                     WHERE session_id = ?1 ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![sid], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut out = Vec::new();
        for (seq, after_ordinal, kind_str, text, created_us) in rows {
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
        let session_param = session_id.as_str().to_string();
        // Serialize the message contents up front: a failure here is a
        // `StorageError::Storage`, which the pool's `anyhow` closure can't
        // build.
        let mut prepared: Vec<(String, String, String, String)> =
            Vec::with_capacity(new_active.len());
        for msg in new_active {
            let content = serde_json::to_string(&msg.content)
                .map_err(|e| StorageError::Storage(format!("serialize message content: {e}")))?;
            prepared.push((
                msg.role.as_str().to_string(),
                content,
                msg.source().as_str().to_string(),
                msg.platform_msg_id().to_string(),
            ));
        }

        self.pool
            .interact("sessions.apply_session_compaction", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // Next ordinal doubles as the supersede pointer: every
                // existing active row points at it, and the first new active
                // message lands there.
                let next_ordinal: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
                    rusqlite::params![session_param],
                    |row| row.get(0),
                )?;

                tx.execute(
                    "UPDATE session_messages SET superseded_by = ?2 \
                     WHERE session_id = ?1 AND superseded_by IS NULL",
                    rusqlite::params![session_param, next_ordinal],
                )?;

                let now_us = super::time::to_us(chrono::Utc::now());
                // Multi-row INSERT, batched under SQLite's 999-bind limit.
                // 7 columns per row → 142 rows per batch leaves 5 spare;
                // typical Summarize emits ≤4 rows so this is one batch in
                // practice. Keeps the whole compaction inside one tx and
                // round-trip count constant (1) instead of O(new_active).
                const COLS_PER_ROW: usize = 7;
                const ROWS_PER_BATCH: usize = 999 / COLS_PER_ROW;
                for (chunk_idx, chunk) in prepared.chunks(ROWS_PER_BATCH).enumerate() {
                    let mut sql = String::from(
                        "INSERT INTO session_messages \
                         (session_id, ordinal, role, content, created_at, source, platform_msg_id) VALUES ",
                    );
                    let mut params: Vec<rusqlite::types::Value> =
                        Vec::with_capacity(chunk.len() * COLS_PER_ROW);
                    for (i, (role, content, source, platform_msg_id)) in chunk.iter().enumerate() {
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
                        params.push(rusqlite::types::Value::Text(session_param.clone()));
                        params.push(rusqlite::types::Value::Integer(ordinal));
                        params.push(rusqlite::types::Value::Text(role.clone()));
                        params.push(rusqlite::types::Value::Text(content.clone()));
                        params.push(rusqlite::types::Value::Integer(now_us));
                        params.push(rusqlite::types::Value::Text(source.clone()));
                        params.push(rusqlite::types::Value::Text(platform_msg_id.clone()));
                    }
                    tx.execute(&sql, rusqlite::params_from_iter(params))?;
                }

                tx.commit()?;
                Ok(())
            })
            .await
    }

    async fn load_active_session_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ChatMessage>> {
        let sid = session_id.as_str().to_string();
        let rows = self
            .pool
            .interact("sessions.load_active_session_messages", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT role, content, source, platform_msg_id FROM session_messages \
                     WHERE session_id = ?1 AND superseded_by IS NULL \
                     ORDER BY ordinal",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![sid], read_message_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        rows.into_iter().map(decode_message_row).collect()
    }

    async fn latest_session_ordinal(&self, session_id: &SessionId) -> Result<Option<i64>> {
        let sid = session_id.as_str().to_string();
        self.pool
            .interact("sessions.latest_session_ordinal", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT MAX(ordinal) FROM session_messages WHERE session_id = ?1",
                        rusqlite::params![sid],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten())
            })
            .await
    }

    async fn active_index_of_ordinal(
        &self,
        session_id: &SessionId,
        ordinal: i64,
    ) -> Result<Option<usize>> {
        let sid = session_id.as_str().to_string();
        // Both sub-selects hit `idx_session_messages_active`
        // (`session_id, ordinal WHERE superseded_by IS NULL`), so this
        // is two index-only counts — the row content is never read.
        let (count, present) = self
            .pool
            .interact("sessions.active_index_of_ordinal", move |conn| {
                let row = conn
                    .query_row(
                        "SELECT \
                           (SELECT COUNT(*) FROM session_messages \
                            WHERE session_id = ?1 AND superseded_by IS NULL AND ordinal < ?2), \
                           EXISTS (SELECT 1 FROM session_messages \
                                   WHERE session_id = ?1 AND superseded_by IS NULL AND ordinal = ?2)",
                        rusqlite::params![sid, ordinal],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow::anyhow!("active_index returned no rows"))?;
                Ok(row)
            })
            .await?;
        if present == 0 {
            return Ok(None);
        }
        Ok(Some(count as usize))
    }

    async fn count_active_messages(&self, session_id: &SessionId) -> Result<usize> {
        let sid = session_id.as_str().to_string();
        let count = self
            .pool
            .interact("sessions.count_active_messages", move |conn| {
                let count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM session_messages \
                         WHERE session_id = ?1 AND superseded_by IS NULL",
                        rusqlite::params![sid],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow::anyhow!("count_active returned no rows"))?;
                Ok(count)
            })
            .await?;
        Ok(count as usize)
    }

    async fn load_active_session_messages_up_to(
        &self,
        session_id: &SessionId,
        up_to_ordinal: i64,
    ) -> Result<Vec<ChatMessage>> {
        let sid = session_id.as_str().to_string();
        let rows = self
            .pool
            .interact("sessions.load_active_session_messages_up_to", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT role, content, source, platform_msg_id FROM session_messages \
                     WHERE session_id = ?1 AND superseded_by IS NULL AND ordinal <= ?2 \
                     ORDER BY ordinal",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![sid, up_to_ordinal], read_message_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        rows.into_iter().map(decode_message_row).collect()
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
        let sid = session_id.as_str().to_string();
        // `before_ordinal IS NULL OR ordinal < before_ordinal` so a
        // single SQL string handles both the "fresh tail" and the
        // scroll-up "next page" calls. The partial active index
        // (`session_id, ordinal WHERE superseded_by IS NULL`) makes
        // the DESC+LIMIT a back-of-the-index walk — never reads more
        // than `limit` row contents off disk even on a million-row
        // session.
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<RawMessageRowWithMeta> = self
            .pool
            .interact("sessions.load_active_session_messages_tail", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ordinal, role, content, source, created_at, platform_msg_id FROM session_messages \
                     WHERE session_id = ?1 AND superseded_by IS NULL \
                       AND (?2 IS NULL OR ordinal < ?2) \
                     ORDER BY ordinal DESC \
                     LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![sid, before_ordinal, limit_i64],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, i64>(4)?,
                                row.get::<_, String>(5)?,
                            ))
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut out: Vec<(i64, DateTime<Utc>, baybo_model::ChatMessage)> = Vec::new();
        for (ordinal, role, content_json, source_str, created_us, platform_msg_id) in rows {
            let created_at = super::time::from_us(created_us).ok_or_else(|| {
                StorageError::Storage(format!(
                    "session_messages.created_at out of range: {created_us}"
                ))
            })?;
            out.push((
                ordinal,
                created_at,
                decode_message_row((role, content_json, source_str, platform_msg_id))?,
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
        let sid = session_id.as_str().to_string();
        // Forward difference: rows with ordinal strictly greater than the
        // client's cursor, capped at `limit`. The partial active index
        // bites the front of the range (`ordinal > N`) so a session
        // with hundreds of older rows pays nothing for them — only the
        // difference window is read.
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<RawMessageRowWithMeta> = self
            .pool
            .interact("sessions.load_active_session_messages_since", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ordinal, role, content, source, created_at, platform_msg_id FROM session_messages \
                     WHERE session_id = ?1 AND superseded_by IS NULL \
                       AND ordinal > ?2 \
                     ORDER BY ordinal ASC \
                     LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![sid, after_ordinal, limit_i64],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, i64>(4)?,
                                row.get::<_, String>(5)?,
                            ))
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut out: Vec<(i64, DateTime<Utc>, baybo_model::ChatMessage)> = Vec::new();
        for (ordinal, role, content_json, source_str, created_us, platform_msg_id) in rows {
            let created_at = super::time::from_us(created_us).ok_or_else(|| {
                StorageError::Storage(format!(
                    "session_messages.created_at out of range: {created_us}"
                ))
            })?;
            out.push((
                ordinal,
                created_at,
                decode_message_row((role, content_json, source_str, platform_msg_id))?,
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
        let sid = session_id.as_str().to_string();
        let platform_msg_id = platform_msg_id.to_string();
        // No superseded filter: a compacted-away row still proves the
        // send was durably persisted, which is all the outbox needs.
        self.pool
            .interact(
                "sessions.find_message_ordinal_by_platform_msg_id",
                move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT ordinal FROM session_messages \
                             WHERE session_id = ?1 AND platform_msg_id = ?2 \
                             ORDER BY ordinal DESC \
                             LIMIT 1",
                            rusqlite::params![sid, platform_msg_id],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()?)
                },
            )
            .await
    }

    async fn load_last_user_message(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(DateTime<Utc>, baybo_model::ChatMessage)>> {
        let sid = session_id.as_str().to_string();
        // Newest human-authored row (source `user` / `user_interjection`,
        // i.e. `from_user`). The partial active index makes `ORDER BY
        // ordinal DESC LIMIT 1` a single back-of-index probe regardless of
        // how many tool/agent rows the turn appended after the prompt.
        let row = self
            .pool
            .interact("sessions.load_last_user_message", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT created_at, role, content, source, platform_msg_id FROM session_messages \
                         WHERE session_id = ?1 AND superseded_by IS NULL \
                           AND source IN ('user', 'user_interjection') \
                         ORDER BY ordinal DESC \
                         LIMIT 1",
                        rusqlite::params![sid],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;

        let Some((created_us, role, content_json, source_str, platform_msg_id)) = row else {
            return Ok(None);
        };
        let created_at = super::time::from_us(created_us).ok_or_else(|| {
            StorageError::Storage(format!(
                "session_messages.created_at out of range: {created_us}"
            ))
        })?;
        Ok(Some((
            created_at,
            decode_message_row((role, content_json, source_str, platform_msg_id))?,
        )))
    }

    async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredMessage>> {
        let sid = session_id.as_str().to_string();
        let rows = self
            .pool
            .interact("sessions.load_session_messages_with_supersede", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ordinal, superseded_by, role, content, created_at, source, platform_msg_id \
                     FROM session_messages \
                     WHERE session_id = ?1 ORDER BY ordinal",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![sid], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut out = Vec::new();
        for (ordinal, superseded_by, role, content_json, created_us, source_str, platform_msg_id) in
            rows
        {
            let created_at = super::time::from_us(created_us).ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!(
                    "session_messages.created_at out of range: {created_us}"
                ))
            })?;
            out.push(StoredMessage {
                ordinal,
                superseded_by,
                created_at,
                message: decode_message_row((role, content_json, source_str, platform_msg_id))?,
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
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    /// Run one raw statement against the pool (test-only schema surgery /
    /// hand-written rows the store's own API can't produce).
    async fn exec(pool: &SqlitePool, sql: &'static str, params: Vec<rusqlite::types::Value>) {
        pool.interact("test.exec", move |conn| {
            conn.execute(sql, rusqlite::params_from_iter(params))?;
            Ok(())
        })
        .await
        .unwrap();
    }

    /// Re-run the schema/migration boot path (what a new binary does against
    /// an older DB).
    async fn init_db(pool: &SqlitePool) {
        pool.interact("test.init_db", super::super::init_db)
            .await
            .unwrap();
    }

    fn text_value(s: &str) -> rusqlite::types::Value {
        rusqlite::types::Value::Text(s.to_string())
    }

    #[tokio::test]
    async fn round_trip_session() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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

        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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
    async fn list_all_skips_undeserializable_row() {
        // A bad row can carry a `lineage.kind` this build doesn't know.
        // `list_all` must skip that one row (log + continue) and still
        // return every good session, rather than erroring the whole listing
        // and 500-ing the CLI picker / web UI.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

        let good = make_root_session("good-1");
        store.save(&good).await.unwrap();

        // Hand-write such a row straight into the table — the current
        // `save` path can't construct one.
        let bad_blob = r#"{
            "id": "bad-lineage",
            "user": {"id": "u1", "name": null, "channel": "tui"},
            "channel": "tui",
            "created_at": "2024-01-01T00:00:00Z",
            "last_active": "2024-01-01T00:00:00Z",
            "state": {},
            "root_session_id": "good-1",
            "trigger": {"kind": "user"},
            "lineage": {"parent_session_id": "good-1", "parent_job_id": "job-x", "kind": "unknown_kind"}
        }"#;
        assert!(
            serde_json::from_str::<Session>(bad_blob).is_err(),
            "the unknown-lineage-kind blob must not deserialize"
        );
        exec(
            &store.pool,
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, created_at, last_active, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                text_value("bad-lineage"),
                text_value("good-1"),
                text_value("user"),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                text_value(bad_blob),
            ],
        )
        .await;

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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
        // telegram rows out of the sqlite round-trip entirely so a
        // gateway hosting thousands of bot sessions doesn't pay an
        // O(all-sessions) cost on every chat-list refresh.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        // (sqlite/mod.rs) handles. Simulate the pre-`pinned` schema by
        // dropping the column the fresh `init_db` created, write a row the
        // way an old build would (no `pinned`), then re-run `init_db` — the
        // boot path a new binary takes. Without the ALTER migration the
        // store's `SELECT … pinned` would fail with "no such column"; with
        // it the column comes back and the old row defaults to unpinned.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        exec(&pool, "ALTER TABLE sessions DROP COLUMN pinned", Vec::new()).await;
        let data = serde_json::to_string(&make_root_session("legacy-1")).unwrap();
        exec(
            &pool,
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, created_at, last_active, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                text_value("legacy-1"),
                text_value("legacy-1"),
                text_value("user"),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Text(data),
            ],
        )
        .await;
        // Re-running init_db applies the idempotent ALTER (re-adds pinned).
        init_db(&pool).await;

        let store = SqliteSessionStore::new(pool);
        let id = SessionId::from("legacy-1");
        let loaded = store.get(&id).await.unwrap().expect("legacy row present");
        assert!(!loaded.pinned, "migrated legacy row defaults to unpinned");
        // And the column is now writable like any other.
        assert!(store.set_pinned(&id, true).await.unwrap());
        assert!(store.get(&id).await.unwrap().unwrap().pinned);
    }

    #[tokio::test]
    async fn message_platform_msg_id_round_trips_and_legacy_defaults_empty() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool.clone());
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

        exec(
            &pool,
            "ALTER TABLE session_messages DROP COLUMN platform_msg_id",
            Vec::new(),
        )
        .await;
        exec(
            &pool,
            "INSERT INTO session_messages \
             (session_id, ordinal, role, content, created_at, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                text_value("platform-msg-id"),
                rusqlite::types::Value::Integer(1),
                text_value("user"),
                rusqlite::types::Value::Text(
                    serde_json::to_string(&vec![baybo_model::ContentBlock::Text("legacy".into())])
                        .unwrap(),
                ),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                text_value("user"),
            ],
        )
        .await;
        init_db(&pool).await;

        let store = SqliteSessionStore::new(pool);
        let loaded = store
            .load_active_session_messages(&session.id)
            .await
            .unwrap();
        assert_eq!(loaded[0].platform_msg_id(), "");
        assert_eq!(loaded[1].platform_msg_id(), "");
    }

    #[tokio::test]
    async fn source_event_append_is_idempotent_across_compaction() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
        let session = make_root_session("source-event");
        store.save(&session).await.unwrap();

        let original =
            baybo_model::ChatMessage::cron_notification(vec![baybo_model::ContentBlock::Text(
                "original".into(),
            )]);
        let replay =
            baybo_model::ChatMessage::cron_notification(vec![baybo_model::ContentBlock::Text(
                "must not replace original".into(),
            )]);
        assert_eq!(
            store
                .append_session_message_idempotent(&session.id, "cron:execution-1", &original)
                .await
                .unwrap(),
            SessionMessageAppendOutcome::Inserted { ordinal: 0 }
        );
        assert_eq!(
            store
                .append_session_message_idempotent(&session.id, "cron:execution-1", &replay)
                .await
                .unwrap(),
            SessionMessageAppendOutcome::Existing { ordinal: 0 }
        );

        store
            .apply_session_compaction(
                &session.id,
                &[baybo_model::ChatMessage::system(vec![
                    baybo_model::ContentBlock::Text("summary".into()),
                ])],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .append_session_message_idempotent(&session.id, "cron:execution-1", &replay)
                .await
                .unwrap(),
            SessionMessageAppendOutcome::Existing { ordinal: 0 },
            "superseding a row must not release its source-event key"
        );

        let rows = store
            .load_session_messages_with_supersede(&session.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "one source row plus one compacted row");
        assert_eq!(rows[0].message.content, original.content);
    }

    #[tokio::test]
    async fn legacy_session_messages_table_gains_source_event_id() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        exec(
            &pool,
            "DROP INDEX idx_session_messages_source_event",
            Vec::new(),
        )
        .await;
        exec(
            &pool,
            "ALTER TABLE session_messages DROP COLUMN source_event_id",
            Vec::new(),
        )
        .await;

        init_db(&pool).await;
        let store = SqliteSessionStore::new(pool);
        let session = make_root_session("source-event-migrated");
        store.save(&session).await.unwrap();
        let message =
            baybo_model::ChatMessage::cron_notification(vec![baybo_model::ContentBlock::Text(
                "migrated".into(),
            )]);
        assert_eq!(
            store
                .append_session_message_idempotent(&session.id, "cron:legacy", &message)
                .await
                .unwrap(),
            SessionMessageAppendOutcome::Inserted { ordinal: 0 }
        );
        assert_eq!(
            store
                .append_session_message_idempotent(&session.id, "cron:legacy", &message)
                .await
                .unwrap(),
            SessionMessageAppendOutcome::Existing { ordinal: 0 }
        );
    }

    #[tokio::test]
    async fn save_does_not_clobber_pinned_set_by_set_pinned() {
        // Same race the `hidden` / `last_llm` guards defend against: a
        // concurrent full-blob `save` (a `touch` on the next inbound
        // message, carrying a stale in-memory `Session` with pinned=false)
        // must NOT wipe a pin set via the targeted `set_pinned`.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
    async fn save_does_not_clobber_archived_set_by_set_archived() {
        // Same race the `hidden` / `pinned` guards defend against: a
        // concurrent full-blob `save` (a `touch` on the next inbound
        // message, carrying a stale in-memory `Session` with
        // archived=false) must NOT wipe a flag set via the targeted
        // `set_archived`.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

        let s = make_root_session("archive-then-save");
        store.save(&s).await.unwrap();
        assert!(store.set_archived(&s.id, true).await.unwrap());

        // Re-save the stale in-memory copy (still archived=false).
        store.save(&s).await.unwrap();

        let loaded = store.get(&s.id).await.unwrap().expect("row present");
        assert!(
            loaded.archived,
            "save must preserve the archived column owned by set_archived"
        );

        // Unarchive clears it back.
        assert!(store.set_archived(&s.id, false).await.unwrap());
        let cleared = store.get(&s.id).await.unwrap().expect("row present");
        assert!(!cleared.archived);

        // Unknown session id reports no row updated.
        assert!(
            !store
                .set_archived(&SessionId::from("nope"), true)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn list_by_channel_reflects_archived_column() {
        // `list_by_channel` / `list_all` must project the flat `archived`
        // column the same way `get` does, so a listed `Session` carries
        // the authoritative flag rather than the (stale) blob value.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

        let mut s = make_root_session("archive-list");
        s.channel = ChannelType::http();
        store.save(&s).await.unwrap();
        assert!(store.set_archived(&s.id, true).await.unwrap());

        let listed = store.list_by_channel(&ChannelType::http()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].archived,
            "archived must reflect the column in list projections"
        );
        let all = store.list_all().await.unwrap();
        assert!(
            all.iter().any(|row| row.id == s.id && row.archived),
            "list_all must project archived too"
        );
    }

    #[tokio::test]
    async fn legacy_sessions_table_without_archived_is_migrated() {
        // The "DB created before `archived` existed" case the migration
        // list (sqlite/mod.rs) handles — same shape as the `pinned`
        // migration test above.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        exec(
            &pool,
            "ALTER TABLE sessions DROP COLUMN archived",
            Vec::new(),
        )
        .await;
        let data = serde_json::to_string(&make_root_session("legacy-arch")).unwrap();
        exec(
            &pool,
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, created_at, last_active, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                text_value("legacy-arch"),
                text_value("legacy-arch"),
                text_value("user"),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Text(data),
            ],
        )
        .await;
        // Re-running init_db applies the idempotent ALTER (re-adds archived).
        init_db(&pool).await;

        let store = SqliteSessionStore::new(pool);
        let id = SessionId::from("legacy-arch");
        let loaded = store.get(&id).await.unwrap().expect("legacy row present");
        assert!(
            !loaded.archived,
            "migrated legacy row defaults to unarchived"
        );
        // And the column is now writable like any other.
        assert!(store.set_archived(&id, true).await.unwrap());
        assert!(store.get(&id).await.unwrap().unwrap().archived);
    }

    #[tokio::test]
    async fn set_folder_round_trips_and_clears() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        // SQLite refuses to drop an indexed column, so drop the index first
        // — this also recreates the genuine pre-folder_id schema (no column,
        // no index), the exact state `init_db`'s migration must recover from.
        exec(
            &pool,
            "DROP INDEX IF EXISTS idx_sessions_folder",
            Vec::new(),
        )
        .await;
        exec(
            &pool,
            "ALTER TABLE sessions DROP COLUMN folder_id",
            Vec::new(),
        )
        .await;
        let data = serde_json::to_string(&make_root_session("legacy-fld")).unwrap();
        exec(
            &pool,
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, created_at, last_active, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                text_value("legacy-fld"),
                text_value("legacy-fld"),
                text_value("user"),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Text(data),
            ],
        )
        .await;
        init_db(&pool).await;

        let store = SqliteSessionStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        exec(&pool, "ALTER TABLE sessions DROP COLUMN title", Vec::new()).await;
        let data = serde_json::to_string(&make_root_session("legacy-title")).unwrap();
        exec(
            &pool,
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, created_at, last_active, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                text_value("legacy-title"),
                text_value("legacy-title"),
                text_value("user"),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Text(data),
            ],
        )
        .await;
        init_db(&pool).await;

        let store = SqliteSessionStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);
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

    #[tokio::test]
    async fn agent_binding_is_seeded_at_insert_and_immutable_after() {
        // No setter exists for `agent_id`/`agent_framework` — the binding
        // is seeded into the INSERT column list at creation, mirroring
        // `hidden`/`pinned`. Proving it round-trips through `save`/`get`
        // AND survives a later full-blob `save` from a stale in-memory
        // copy (carrying a *different* binding) simultaneously proves the
        // INSERT-seeding and the `DO UPDATE` omission: there is no code
        // path, past creation, that can write these columns.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

        let id = AgentProfileId::from("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let mut s = make_root_session("agent-bound");
        s.state.agent_id = Some(id.clone());
        s.state.agent_framework = Some(AgentFramework::Baybo);
        store.save(&s).await.unwrap();

        let loaded = store.get(&s.id).await.unwrap().expect("row present");
        assert_eq!(loaded.state.agent_id, Some(id.clone()));
        assert_eq!(loaded.state.agent_framework, Some(AgentFramework::Baybo));

        // Re-save a stale in-memory copy carrying a *different* binding —
        // e.g. a concurrent creation-race loser's own seeded values.
        // `save`'s DO UPDATE must not clobber the columns this row was
        // created with.
        let mut mutated = s.clone();
        mutated.state.agent_id = Some(AgentProfileId::from("some-other-id"));
        mutated.state.agent_framework = Some(AgentFramework::Codex);
        store.save(&mutated).await.unwrap();

        let reloaded = store.get(&s.id).await.unwrap().expect("row present");
        assert_eq!(
            reloaded.state.agent_id,
            Some(id),
            "save must not clobber the agent binding columns seeded at INSERT"
        );
        assert_eq!(reloaded.state.agent_framework, Some(AgentFramework::Baybo));

        // Unbound session reads back as None.
        let plain = make_root_session("plain");
        store.save(&plain).await.unwrap();
        let loaded = store.get(&plain.id).await.unwrap().expect("row present");
        assert_eq!(loaded.state.agent_id, None);
        assert_eq!(loaded.state.agent_framework, None);
    }

    #[tokio::test]
    async fn list_projections_reflect_agent_binding_columns() {
        // `list_all` / `list_by_channel` must project the flat
        // `agent_id` / `agent_framework` columns the same way `get`
        // does, so a listed `Session` carries the authoritative
        // binding rather than the (stale, always-`None`) blob value.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

        let mut s = make_root_session("agent-list");
        s.channel = ChannelType::http();
        let id = AgentProfileId::from("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        s.state.agent_id = Some(id.clone());
        s.state.agent_framework = Some(AgentFramework::Codex);
        store.save(&s).await.unwrap();

        let listed_all = store.list_all().await.unwrap();
        let found = listed_all.iter().find(|x| x.id == s.id).unwrap();
        assert_eq!(found.state.agent_id, Some(id.clone()));
        assert_eq!(found.state.agent_framework, Some(AgentFramework::Codex));

        let listed_by_channel = store.list_by_channel(&ChannelType::http()).await.unwrap();
        assert_eq!(listed_by_channel.len(), 1);
        assert_eq!(listed_by_channel[0].state.agent_id, Some(id));
        assert_eq!(
            listed_by_channel[0].state.agent_framework,
            Some(AgentFramework::Codex)
        );
    }

    #[tokio::test]
    async fn legacy_sessions_table_without_agent_binding_is_migrated() {
        // The "DB created before `agent_id`/`agent_framework` existed" case
        // the migration list (sqlite/mod.rs) handles. Without the ALTER
        // migrations the store's `SELECT … agent_id, agent_framework` would
        // fail with "no such column"; with them the columns come back NULL
        // and the old row reads as unbound.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        exec(
            &pool,
            "ALTER TABLE sessions DROP COLUMN agent_id",
            Vec::new(),
        )
        .await;
        exec(
            &pool,
            "ALTER TABLE sessions DROP COLUMN agent_framework",
            Vec::new(),
        )
        .await;
        let data = serde_json::to_string(&make_root_session("legacy-agent")).unwrap();
        exec(
            &pool,
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, created_at, last_active, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                text_value("legacy-agent"),
                text_value("legacy-agent"),
                text_value("user"),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Text(data),
            ],
        )
        .await;
        // Re-running init_db applies the idempotent ALTERs (re-adds both
        // columns).
        init_db(&pool).await;

        let store = SqliteSessionStore::new(pool);
        let id = SessionId::from("legacy-agent");
        let loaded = store.get(&id).await.unwrap().expect("legacy row present");
        assert_eq!(loaded.state.agent_id, None);
        assert_eq!(loaded.state.agent_framework, None);
        // And the columns are now seedable like any other, via the ordinary
        // creation-time INSERT — there is no separate setter, migrated or
        // not.
        let agent_id = AgentProfileId::from("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let mut fresh = make_root_session("legacy-agent-fresh");
        fresh.state.agent_id = Some(agent_id.clone());
        fresh.state.agent_framework = Some(AgentFramework::Baybo);
        store.save(&fresh).await.unwrap();
        assert_eq!(
            store.get(&fresh.id).await.unwrap().unwrap().state.agent_id,
            Some(agent_id)
        );
    }

    #[tokio::test]
    async fn unknown_agent_framework_column_errors_rather_than_falling_back() {
        // Matches the agent-profiles read rule: an unrecognised framework
        // tag must surface as a hard `StorageError`, never silently
        // degrade to a default framework.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let s = make_root_session("bad-framework");
        exec(
            &pool,
            "INSERT INTO sessions \
             (id, root_session_id, trigger_kind, created_at, last_active, data, \
              agent_id, agent_framework) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            vec![
                text_value(s.id.as_str()),
                text_value(s.id.as_str()),
                text_value("user"),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Integer(super::super::time::to_us(Utc::now())),
                rusqlite::types::Value::Text(serde_json::to_string(&s).unwrap()),
                text_value("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                text_value("not_a_real_framework"),
            ],
        )
        .await;

        let store = SqliteSessionStore::new(pool);
        let err = store.get(&s.id).await.unwrap_err();
        assert!(
            matches!(err, StorageError::Storage(ref msg) if msg.contains("unknown agent_framework")),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn list_all_skips_unknown_agent_framework_row_but_get_still_hard_errors() {
        // Lists must skip-and-warn a corrupt `agent_framework` row exactly
        // like they already skip an undeserializable JSON blob — one broken
        // row must not 500 the whole chat-list query. `get()` on that same
        // row keeps the hard error: a caller asking for one specific session
        // needs to know it's broken, not have it silently vanish.
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionStore::new(pool);

        let good = make_root_session("good-framework");
        store.save(&good).await.unwrap();
        let bad = make_root_session("bad-framework");
        store.save(&bad).await.unwrap();
        exec(
            &store.pool,
            "UPDATE sessions SET agent_id = ?2, agent_framework = ?3 WHERE id = ?1",
            vec![
                text_value("bad-framework"),
                text_value("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                text_value("not_a_real_framework"),
            ],
        )
        .await;

        let listed = store.list_all().await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(&"good-framework"),
            "unaffected row must survive the listing: {ids:?}"
        );
        assert!(
            !ids.contains(&"bad-framework"),
            "corrupt-framework row must be skipped, not fail the listing: {ids:?}"
        );

        let err = store.get(&bad.id).await.unwrap_err();
        assert!(
            matches!(err, StorageError::Storage(ref msg) if msg.contains("unknown agent_framework")),
            "unexpected error: {err:?}"
        );
    }
}
