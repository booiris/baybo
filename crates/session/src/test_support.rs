//! In-memory `SessionStore` / `SessionSummaryStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so they never ship in
//! release builds. Live in `baybo-session` (next to the traits they
//! implement) so crates that depend on `baybo-session` but not on
//! `baybo-storage` can still spin up fake stores for unit tests.

use std::collections::HashMap;

use async_trait::async_trait;
use baybo_model::{
    ChannelType, ChatMessage, ControlEvent, ControlEventKind, FolderId, LineageKind, Session,
    SessionId,
};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use baybo_store::StorageError;
use baybo_store::session::{Result, SessionStore, StoredMessage};
use baybo_store::session_folder::{SessionFolderRow, SessionFolderStore};
use baybo_store::session_summary::{SessionSummaryRow, SessionSummaryStore};

/// One stored row in the in-memory session transcript log — mirrors
/// the libsql layout closely enough that `apply_session_compaction`
/// can supersede rows the same way the real backend does.
#[derive(Clone)]
struct StoredMessageRow {
    ordinal: u64,
    message: ChatMessage,
    superseded_by: Option<u64>,
    created_at: DateTime<Utc>,
}

/// In-memory `SessionStore` for tests across the workspace. Lineage
/// columns are stubbed (`list_lineage_children` returns empty) — tests
/// that need that surface should use the real libsql store via
/// `baybo_storage::Store::open` against a tempfile.
#[derive(Default)]
pub struct MemorySessionStore {
    data: Mutex<HashMap<SessionId, Session>>,
    transcripts: Mutex<HashMap<SessionId, Vec<StoredMessageRow>>>,
    control_events: Mutex<HashMap<SessionId, Vec<ControlEvent>>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronously seed a session row so consumers (`SessionStore::get`,
    /// `SessionManager::history`, the `on_session_end` memory hook, …)
    /// find it without going through an async executor — useful from
    /// sync test setup (`AgentTestHarnessBuilder::build`).
    pub fn seed_session(&self, session: &Session) {
        self.data.lock().insert(session.id.clone(), session.clone());
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Session>> {
        Ok(self.data.lock().get(session_id).cloned())
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let mut data = self.data.lock();
        let mut to_store = session.clone();
        // `hidden` / `pinned` / `folder_id` are owned by their targeted
        // setters; preserve the existing row's values so a stale in-memory
        // save can't un-hide, un-pin, or re-file it. Mirrors the libsql
        // impl, whose upsert omits all three flat columns.
        if let Some(existing) = data.get(&session.id) {
            to_store.hidden = existing.hidden;
            to_store.pinned = existing.pinned;
            to_store.folder_id = existing.folder_id.clone();
        }
        data.insert(session.id.clone(), to_store);
        Ok(())
    }

    async fn set_hidden(&self, session_id: &SessionId, hidden: bool) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.hidden = hidden;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_pinned(&self, session_id: &SessionId, pinned: bool) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.pinned = pinned;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_folder(
        &self,
        session_id: &SessionId,
        folder_id: Option<&baybo_model::FolderId>,
    ) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.folder_id = folder_id.cloned();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_last_llm(
        &self,
        session_id: &SessionId,
        llm: Option<&baybo_model::LlmEntryName>,
    ) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.state.last_llm = llm.cloned();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        self.transcripts.lock().remove(session_id);
        Ok(self.data.lock().remove(session_id).is_some())
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<SessionId>> {
        Ok(self
            .data
            .lock()
            .values()
            .filter(|s| s.last_active < before)
            .map(|s| s.id.clone())
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<Session>> {
        Ok(self.data.lock().values().cloned().collect())
    }

    async fn list_by_channel(&self, channel: &ChannelType) -> Result<Vec<Session>> {
        Ok(self
            .data
            .lock()
            .values()
            .filter(|s| &s.channel == channel)
            .cloned()
            .collect())
    }

    async fn list_lineage_children(
        &self,
        _parent_session_id: &SessionId,
    ) -> Result<Vec<(SessionId, LineageKind)>> {
        Ok(Vec::new())
    }

    async fn append_session_message(
        &self,
        session_id: &SessionId,
        message: &ChatMessage,
    ) -> Result<i64> {
        let mut guard = self.transcripts.lock();
        let log = guard.entry(session_id.clone()).or_default();
        let ordinal = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
        log.push(StoredMessageRow {
            ordinal,
            message: message.clone(),
            superseded_by: None,
            created_at: Utc::now(),
        });
        i64::try_from(ordinal).map_err(|_| {
            StorageError::Internal(anyhow::anyhow!("ordinal {ordinal} exceeds i64::MAX"))
        })
    }

    async fn append_control_event(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        created_at: DateTime<Utc>,
    ) -> Result<i64> {
        let mut guard = self.control_events.lock();
        let log = guard.entry(session_id.clone()).or_default();
        let seq = log.last().map(|e| e.seq + 1).unwrap_or(0);
        log.push(ControlEvent {
            seq,
            after_ordinal,
            kind,
            text: text.to_string(),
            created_at,
        });
        Ok(seq)
    }

    async fn list_control_events(&self, session_id: &SessionId) -> Result<Vec<ControlEvent>> {
        Ok(self
            .control_events
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn apply_session_compaction(
        &self,
        session_id: &SessionId,
        new_active: &[ChatMessage],
    ) -> Result<()> {
        let mut guard = self.transcripts.lock();
        let log = guard.entry(session_id.clone()).or_default();
        let next_ordinal = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
        for entry in log.iter_mut() {
            if entry.superseded_by.is_none() {
                entry.superseded_by = Some(next_ordinal);
            }
        }
        let stamp = Utc::now();
        for (offset, msg) in new_active.iter().enumerate() {
            log.push(StoredMessageRow {
                ordinal: next_ordinal + offset as u64,
                message: msg.clone(),
                superseded_by: None,
                created_at: stamp,
            });
        }
        Ok(())
    }

    async fn load_active_session_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ChatMessage>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| {
                let mut active: Vec<&StoredMessageRow> =
                    log.iter().filter(|m| m.superseded_by.is_none()).collect();
                active.sort_by_key(|m| m.ordinal);
                active.into_iter().map(|m| m.message.clone()).collect()
            })
            .unwrap_or_default())
    }

    async fn latest_session_ordinal(&self, session_id: &SessionId) -> Result<Option<i64>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .and_then(|log| log.iter().map(|m| m.ordinal as i64).max()))
    }

    async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredMessage>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| {
                let mut rows: Vec<_> = log
                    .iter()
                    .map(|m| StoredMessage {
                        ordinal: m.ordinal as i64,
                        superseded_by: m.superseded_by.map(|v| v as i64),
                        created_at: m.created_at,
                        message: m.message.clone(),
                    })
                    .collect();
                rows.sort_by_key(|m| m.ordinal);
                rows
            })
            .unwrap_or_default())
    }

    async fn active_index_of_ordinal(
        &self,
        session_id: &SessionId,
        ordinal: i64,
    ) -> Result<Option<usize>> {
        Ok(self.transcripts.lock().get(session_id).and_then(|log| {
            let mut active: Vec<&StoredMessageRow> =
                log.iter().filter(|m| m.superseded_by.is_none()).collect();
            active.sort_by_key(|m| m.ordinal);
            active.iter().position(|m| (m.ordinal as i64) == ordinal)
        }))
    }

    async fn count_active_messages(&self, session_id: &SessionId) -> Result<usize> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| log.iter().filter(|m| m.superseded_by.is_none()).count())
            .unwrap_or(0))
    }

    async fn load_active_session_messages_up_to(
        &self,
        session_id: &SessionId,
        up_to_ordinal: i64,
    ) -> Result<Vec<ChatMessage>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| {
                let mut active: Vec<&StoredMessageRow> = log
                    .iter()
                    .filter(|m| m.superseded_by.is_none() && (m.ordinal as i64) <= up_to_ordinal)
                    .collect();
                active.sort_by_key(|m| m.ordinal);
                active.into_iter().map(|m| m.message.clone()).collect()
            })
            .unwrap_or_default())
    }

    async fn load_active_session_messages_tail(
        &self,
        session_id: &SessionId,
        before_ordinal: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, ChatMessage)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| {
                let mut active: Vec<&StoredMessageRow> = log
                    .iter()
                    .filter(|m| {
                        m.superseded_by.is_none()
                            && before_ordinal.is_none_or(|b| (m.ordinal as i64) < b)
                    })
                    .collect();
                active.sort_by_key(|m| m.ordinal);
                let skip = active.len().saturating_sub(limit);
                active
                    .into_iter()
                    .skip(skip)
                    .map(|m| (m.ordinal as i64, m.created_at, m.message.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn load_active_session_messages_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, ChatMessage)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| {
                let mut active: Vec<&StoredMessageRow> = log
                    .iter()
                    .filter(|m| m.superseded_by.is_none() && (m.ordinal as i64) > after_ordinal)
                    .collect();
                active.sort_by_key(|m| m.ordinal);
                active
                    .into_iter()
                    .take(limit)
                    .map(|m| (m.ordinal as i64, m.created_at, m.message.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn find_message_ordinal_by_platform_msg_id(
        &self,
        session_id: &SessionId,
        platform_msg_id: &str,
    ) -> Result<Option<i64>> {
        if platform_msg_id.is_empty() {
            return Ok(None);
        }
        Ok(self.transcripts.lock().get(session_id).and_then(|log| {
            log.iter()
                .filter(|m| m.message.platform_msg_id() == platform_msg_id)
                .max_by_key(|m| m.ordinal)
                .map(|m| m.ordinal as i64)
        }))
    }

    async fn load_last_user_message(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(DateTime<Utc>, ChatMessage)>> {
        Ok(self.transcripts.lock().get(session_id).and_then(|log| {
            log.iter()
                .filter(|m| m.superseded_by.is_none() && m.message.from_user())
                .max_by_key(|m| m.ordinal)
                .map(|m| (m.created_at, m.message.clone()))
        }))
    }
}

/// In-memory `SessionSummaryStore` for tests across the workspace.
/// Mirrors the libsql backend's behaviour for the trait surface
/// (`upsert_success` resets `error_count`, `bump_error_count` inserts
/// a zero row when missing) so unit tests can assert against the same
/// invariants production exercises.
#[derive(Default)]
pub struct MemorySessionSummaryStore {
    rows: Mutex<HashMap<SessionId, SessionSummaryRow>>,
}

impl MemorySessionSummaryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionSummaryStore for MemorySessionSummaryStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<SessionSummaryRow>> {
        Ok(self.rows.lock().get(session_id).cloned())
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
        let mut guard = self.rows.lock();
        let entry = guard
            .entry(session_id.clone())
            .or_insert_with(|| SessionSummaryRow {
                session_id: session_id.clone(),
                cursor: 0,
                pass_count: 0,
                updated_at,
                cost_micros: 0,
                model_id: String::new(),
                span_id: String::new(),
                error_count: 0,
            });
        entry.cursor = cursor;
        entry.pass_count += 1;
        entry.cost_micros += cost_micros_delta;
        entry.model_id = model_id.to_string();
        entry.span_id = span_id.to_string();
        entry.updated_at = updated_at;
        entry.error_count = 0;
        Ok(())
    }

    async fn bump_error_count(
        &self,
        session_id: &SessionId,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut guard = self.rows.lock();
        let entry = guard
            .entry(session_id.clone())
            .or_insert_with(|| SessionSummaryRow {
                session_id: session_id.clone(),
                cursor: 0,
                pass_count: 0,
                updated_at,
                cost_micros: 0,
                model_id: String::new(),
                span_id: String::new(),
                error_count: 0,
            });
        entry.error_count += 1;
        entry.model_id = model_id.to_string();
        entry.span_id = span_id.to_string();
        entry.updated_at = updated_at;
        Ok(())
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        Ok(self.rows.lock().remove(session_id).is_some())
    }

    async fn list_session_ids(&self) -> Result<Vec<SessionId>> {
        Ok(self.rows.lock().keys().cloned().collect())
    }
}

/// In-memory `SessionFolderStore` for tests. Folder structure only —
/// `delete` promotes sub-folders and removes the row but cannot reach a
/// sibling `SessionStore` to null member sessions (it returns an empty
/// affected list). Tests asserting the session-nulling side of dissolve
/// should use the real libsql store via a tempfile.
#[derive(Default)]
pub struct MemorySessionFolderStore {
    rows: Mutex<HashMap<FolderId, SessionFolderRow>>,
}

impl MemorySessionFolderStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionFolderStore for MemorySessionFolderStore {
    async fn list(&self) -> Result<Vec<SessionFolderRow>> {
        let mut rows: Vec<SessionFolderRow> = self.rows.lock().values().cloned().collect();
        rows.sort_by(|a, b| {
            (
                a.parent_id.as_ref().map(|p| p.as_str().to_owned()),
                a.position,
            )
                .cmp(&(
                    b.parent_id.as_ref().map(|p| p.as_str().to_owned()),
                    b.position,
                ))
        });
        Ok(rows)
    }

    async fn get(&self, id: &FolderId) -> Result<Option<SessionFolderRow>> {
        Ok(self.rows.lock().get(id).cloned())
    }

    async fn create(&self, row: &SessionFolderRow) -> Result<()> {
        self.rows.lock().insert(row.id.clone(), row.clone());
        Ok(())
    }

    async fn rename(&self, id: &FolderId, name: &str) -> Result<bool> {
        let mut rows = self.rows.lock();
        match rows.get_mut(id) {
            Some(r) => {
                r.name = name.to_owned();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn reparent(
        &self,
        id: &FolderId,
        parent_id: Option<&FolderId>,
        position: i64,
    ) -> Result<bool> {
        let mut rows = self.rows.lock();
        match rows.get_mut(id) {
            Some(r) => {
                r.parent_id = parent_id.cloned();
                r.position = position;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn reorder(&self, parent_id: Option<&FolderId>, ordered_ids: &[FolderId]) -> Result<()> {
        let mut rows = self.rows.lock();
        for (idx, id) in ordered_ids.iter().enumerate() {
            if let Some(r) = rows.get_mut(id)
                && r.parent_id.as_ref() == parent_id
            {
                r.position = idx as i64;
            }
        }
        Ok(())
    }

    async fn delete(&self, id: &FolderId) -> Result<Option<Vec<SessionId>>> {
        let mut rows = self.rows.lock();
        if !rows.contains_key(id) {
            return Ok(None);
        }
        for row in rows.values_mut() {
            if row.parent_id.as_ref() == Some(id) {
                row.parent_id = None;
            }
        }
        rows.remove(id);
        Ok(Some(Vec::new()))
    }
}
