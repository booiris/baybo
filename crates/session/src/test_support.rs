//! In-memory `SessionStore` for downstream tests.
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
use baybo_store::session::{
    DreamCandidate, Result, SessionMessageAppendOutcome, SessionStore, StoredMessage,
};
use baybo_store::session_folder::{SessionFolderRow, SessionFolderStore};

/// One stored row in the in-memory session transcript log — mirrors
/// the sqlite layout closely enough that `apply_session_compaction`
/// can supersede rows the same way the real backend does.
#[derive(Clone)]
struct StoredMessageRow {
    ordinal: u64,
    message: ChatMessage,
    superseded_by: Option<u64>,
    created_at: DateTime<Utc>,
    source_event_id: Option<String>,
    /// `true` for a row `apply_session_compaction` wrote (summary head +
    /// re-injected recent turns). The display reads hide these; mirrors the
    /// sqlite `session_messages.compaction_inserted` column.
    compaction_inserted: bool,
}

/// In-memory `SessionStore` for tests across the workspace. Lineage
/// columns are stubbed (`list_lineage_children` returns empty) — tests
/// that need that surface should use the real sqlite store via
/// `baybo_storage::Store::open` against a tempfile.
#[derive(Default)]
pub struct MemorySessionStore {
    data: Mutex<HashMap<SessionId, Session>>,
    transcripts: Mutex<HashMap<SessionId, Vec<StoredMessageRow>>>,
    control_events: Mutex<HashMap<SessionId, Vec<ControlEvent>>>,
    read_cursors: Mutex<HashMap<SessionId, i64>>,
    /// Mirrors the `sessions.dreamed_through_ordinal` flat column.
    dream_cursors: Mutex<HashMap<SessionId, i64>>,
    /// Fault injection: when set, every `append_session_message` fails. Lets a
    /// test drive the paths that must treat an unpersisted row as a failed
    /// write rather than a silent success — a transcript append is the one
    /// store call several delivery guarantees rest on.
    fail_appends: std::sync::atomic::AtomicBool,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent transcript append fail.
    pub fn fail_appends(&self, fail: bool) {
        self.fail_appends
            .store(fail, std::sync::atomic::Ordering::Relaxed);
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
        // Flat columns are owned by targeted setters; preserve them across
        // stale full-blob saves.
        if let Some(existing) = data.get(&session.id) {
            to_store.hidden = existing.hidden;
            to_store.pinned = existing.pinned;
            to_store.archived = existing.archived;
            to_store.folder_id = existing.folder_id.clone();
            to_store.title = existing.title.clone();
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

    async fn set_archived(&self, session_id: &SessionId, archived: bool) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.archived = archived;
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

    async fn set_read_cursor(&self, session_id: &SessionId, ordinal: i64) -> Result<bool> {
        if !self.data.lock().contains_key(session_id) {
            return Ok(false);
        }
        let mut cursors = self.read_cursors.lock();
        let entry = cursors.entry(session_id.clone()).or_insert(ordinal);
        if ordinal > *entry {
            *entry = ordinal;
        }
        Ok(true)
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

    async fn set_last_model(&self, session_id: &SessionId, model: Option<&str>) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.state.last_model = model.map(str::to_string);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_last_effort(&self, session_id: &SessionId, effort: Option<&str>) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.state.last_effort = effort.map(str::to_string);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_title(&self, session_id: &SessionId, title: Option<&str>) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.title = title.map(|t| t.to_string());
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
        if self.fail_appends.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StorageError::Internal(anyhow::anyhow!(
                "injected transcript append failure"
            )));
        }
        let mut guard = self.transcripts.lock();
        let log = guard.entry(session_id.clone()).or_default();
        let ordinal = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
        log.push(StoredMessageRow {
            ordinal,
            message: message.clone(),
            superseded_by: None,
            created_at: Utc::now(),
            source_event_id: None,
            compaction_inserted: false,
        });
        i64::try_from(ordinal).map_err(|_| {
            StorageError::Internal(anyhow::anyhow!("ordinal {ordinal} exceeds i64::MAX"))
        })
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
        if self.fail_appends.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StorageError::Internal(anyhow::anyhow!(
                "injected transcript append failure"
            )));
        }

        let mut guard = self.transcripts.lock();
        let log = guard.entry(session_id.clone()).or_default();
        if let Some(existing) = log
            .iter()
            .find(|row| row.source_event_id.as_deref() == Some(source_event_id))
        {
            let ordinal = i64::try_from(existing.ordinal).map_err(|_| {
                StorageError::Internal(anyhow::anyhow!(
                    "ordinal {} exceeds i64::MAX",
                    existing.ordinal
                ))
            })?;
            return Ok(SessionMessageAppendOutcome::Existing { ordinal });
        }

        let ordinal = log.last().map(|row| row.ordinal + 1).unwrap_or(0);
        let stored_ordinal = i64::try_from(ordinal).map_err(|_| {
            StorageError::Internal(anyhow::anyhow!("ordinal {ordinal} exceeds i64::MAX"))
        })?;
        log.push(StoredMessageRow {
            ordinal,
            message: message.clone(),
            superseded_by: None,
            created_at: Utc::now(),
            source_event_id: Some(source_event_id.to_string()),
            compaction_inserted: false,
        });
        Ok(SessionMessageAppendOutcome::Inserted {
            ordinal: stored_ordinal,
        })
    }

    async fn find_message_ordinal_by_source_event_id(
        &self,
        session_id: &SessionId,
        source_event_id: &str,
    ) -> Result<Option<i64>> {
        self.transcripts
            .lock()
            .get(session_id)
            .and_then(|log| {
                log.iter()
                    .find(|row| row.source_event_id.as_deref() == Some(source_event_id))
                    .map(|row| row.ordinal)
            })
            .map(i64::try_from)
            .transpose()
            .map_err(|error| {
                StorageError::Internal(anyhow::anyhow!(
                    "source-event ordinal exceeds i64::MAX: {error}"
                ))
            })
    }

    async fn append_control_event(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        created_at: DateTime<Utc>,
        platform_msg_id: &str,
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
            platform_msg_id: platform_msg_id.to_string(),
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

    async fn list_control_events_in_range(
        &self,
        session_id: &SessionId,
        lower: i64,
        upper: i64,
    ) -> Result<Vec<ControlEvent>> {
        Ok(self
            .control_events
            .lock()
            .get(session_id)
            .map(|events| {
                events
                    .iter()
                    .filter(|ev| ev.after_ordinal >= lower && ev.after_ordinal <= upper)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn apply_session_compaction(
        &self,
        session_id: &SessionId,
        new_active: &[ChatMessage],
    ) -> Result<i64> {
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
                source_event_id: None,
                compaction_inserted: true,
            });
        }
        Ok(next_ordinal as i64)
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
        self.load_session_messages_with_supersede_since(session_id, i64::MIN)
            .await
    }

    async fn load_session_messages_with_supersede_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
    ) -> Result<Vec<StoredMessage>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| {
                let mut rows: Vec<_> = log
                    .iter()
                    .filter(|m| (m.ordinal as i64) > after_ordinal)
                    .map(|m| StoredMessage {
                        ordinal: m.ordinal as i64,
                        superseded_by: m.superseded_by.map(|v| v as i64),
                        created_at: m.created_at,
                        compaction_inserted: m.compaction_inserted,
                        message: m.message.clone(),
                    })
                    .collect();
                rows.sort_by_key(|m| m.ordinal);
                rows
            })
            .unwrap_or_default())
    }

    async fn supersede_watermark(&self, session_id: &SessionId) -> Result<Option<i64>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .and_then(|log| log.iter().filter_map(|m| m.superseded_by).max())
            .map(|v| v as i64))
    }

    async fn session_created_times(
        &self,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<chrono::DateTime<chrono::Utc>>> {
        Ok(self
            .data
            .lock()
            .values()
            .map(|s| s.created_at)
            .filter(|t| *t >= from && *t < to)
            .collect())
    }

    async fn dream_candidates(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DreamCandidate>> {
        let transcripts = self.transcripts.lock();
        let sessions = self.data.lock();
        let cursors = self.dream_cursors.lock();
        let mut out: Vec<DreamCandidate> = transcripts
            .iter()
            .filter_map(|(id, log)| {
                let session = sessions.get(id);
                let watermark = cursors.get(id).copied();
                let live = log.iter().filter(|m| !m.compaction_inserted);
                // Mirrors the sqlite arms: an ordinal cursor once the
                // conversation has been offered, the time window before that.
                let unread: Vec<_> = match watermark {
                    Some(w) => live.filter(|m| m.ordinal as i64 > w).collect(),
                    None => live
                        .filter(|m| {
                            m.message.from_user() && m.created_at >= since && m.created_at < until
                        })
                        .collect(),
                };
                if unread.is_empty() {
                    return None;
                }
                Some(DreamCandidate {
                    session_id: id.clone(),
                    agent_id: session.and_then(|s| s.state.agent_id.clone()),
                    title: session.and_then(|s| s.title.clone()),
                    last_activity_at: unread.iter().map(|m| m.created_at).max()?,
                    human_message_count: unread.iter().filter(|m| m.message.from_user()).count()
                        as i64,
                    read_from_ordinal: unread.iter().map(|m| m.ordinal as i64).min()?,
                    latest_ordinal: log.iter().map(|m| m.ordinal as i64).max()?,
                })
            })
            .collect();
        out.sort_by(|a, b| b.last_activity_at.cmp(&a.last_activity_at));
        Ok(out)
    }

    async fn set_dreamed_through_ordinal(
        &self,
        session_id: &SessionId,
        ordinal: i64,
    ) -> Result<bool> {
        if !self.data.lock().contains_key(session_id) {
            return Ok(false);
        }
        let mut cursors = self.dream_cursors.lock();
        let entry = cursors.entry(session_id.clone()).or_insert(ordinal);
        if ordinal > *entry {
            *entry = ordinal;
        }
        Ok(true)
    }

    async fn last_user_messages(
        &self,
        session_ids: &[SessionId],
    ) -> Result<Vec<(SessionId, DateTime<Utc>, ChatMessage)>> {
        let transcripts = self.transcripts.lock();
        Ok(session_ids
            .iter()
            .filter_map(|id| {
                transcripts.get(id).and_then(|log| {
                    log.iter()
                        .filter(|m| !m.compaction_inserted && m.message.from_user())
                        .max_by_key(|m| m.ordinal)
                        .map(|m| (id.clone(), m.created_at, m.message.clone()))
                })
            })
            .collect())
    }

    async fn active_tails(
        &self,
        session_ids: &[SessionId],
        limit: usize,
    ) -> Result<Vec<(SessionId, i64, DateTime<Utc>, ChatMessage)>> {
        let transcripts = self.transcripts.lock();
        let mut out = Vec::new();
        for id in session_ids {
            if let Some(log) = transcripts.get(id) {
                let mut active: Vec<_> = log.iter().filter(|m| !m.compaction_inserted).collect();
                active.sort_by_key(|m| m.ordinal);
                let start = active.len().saturating_sub(limit);
                out.extend(active[start..].iter().map(|m| {
                    (
                        id.clone(),
                        m.ordinal as i64,
                        m.created_at,
                        m.message.clone(),
                    )
                }));
            }
        }
        Ok(out)
    }

    async fn unread_scan(
        &self,
        session_ids: &[SessionId],
        limit: usize,
    ) -> Result<Vec<(SessionId, ChatMessage)>> {
        let transcripts = self.transcripts.lock();
        let cursors = self.read_cursors.lock();
        let mut out = Vec::new();
        for id in session_ids {
            let cursor = cursors.get(id).copied().unwrap_or(-1);
            if let Some(log) = transcripts.get(id) {
                let mut active: Vec<_> = log
                    .iter()
                    .filter(|m| !m.compaction_inserted && (m.ordinal as i64) > cursor)
                    .collect();
                active.sort_by_key(|m| m.ordinal);
                out.extend(
                    active
                        .into_iter()
                        .take(limit)
                        .map(|m| (id.clone(), m.message.clone())),
                );
            }
        }
        Ok(out)
    }

    async fn session_titles(
        &self,
        session_ids: &[SessionId],
    ) -> Result<Vec<(SessionId, Option<String>)>> {
        let data = self.data.lock();
        Ok(session_ids
            .iter()
            .filter_map(|id| data.get(id).map(|s| (id.clone(), s.title.clone())))
            .collect())
    }

    async fn session_channels(
        &self,
        session_ids: &[SessionId],
    ) -> Result<Vec<(SessionId, String)>> {
        let data = self.data.lock();
        Ok(session_ids
            .iter()
            .filter_map(|id| {
                data.get(id)
                    .map(|s| (id.clone(), s.channel.as_str().to_string()))
            })
            .collect())
    }

    async fn touch_last_active(&self, session_id: &SessionId, now: DateTime<Utc>) -> Result<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.last_active = now;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn count_sessions(&self) -> Result<usize> {
        Ok(self.data.lock().len())
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
                        !m.compaction_inserted
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
                    .filter(|m| !m.compaction_inserted && (m.ordinal as i64) > after_ordinal)
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

    async fn compaction_boundaries(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<(i64, DateTime<Utc>)>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .map(|log| {
                let watermarks: std::collections::BTreeSet<u64> =
                    log.iter().filter_map(|m| m.superseded_by).collect();
                watermarks
                    .into_iter()
                    .filter_map(|w| {
                        log.iter()
                            .find(|m| m.ordinal == w)
                            .map(|m| (w as i64, m.created_at))
                    })
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
}

/// In-memory `SessionFolderStore` for tests. Folder structure only —
/// `delete` promotes sub-folders and removes the row but cannot reach a
/// sibling `SessionStore` to null member sessions (it returns an empty
/// affected list). Tests asserting the session-nulling side of dissolve
/// should use the real sqlite store via a tempfile.
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

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{SessionState, TriggerSource, User};

    fn make_session(id: &str) -> Session {
        let id = SessionId::from(id);
        Session {
            id: id.clone(),
            user: User {
                id: "u1".to_string(),
                name: None,
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

    #[tokio::test]
    async fn save_preserves_flat_columns_like_sqlite() {
        // The fake must mirror the sqlite upsert, whose DO UPDATE omits
        // the flat columns owned by the targeted setters — a stale
        // in-memory re-save must not un-hide, un-pin, or un-archive.
        let store = MemorySessionStore::new();
        let s = make_session("preserve-me");
        store.save(&s).await.unwrap();
        assert!(store.set_hidden(&s.id, true).await.unwrap());
        assert!(store.set_pinned(&s.id, true).await.unwrap());
        assert!(store.set_archived(&s.id, true).await.unwrap());

        // Re-save the stale copy (all flags still false).
        store.save(&s).await.unwrap();

        let loaded = store.get(&s.id).await.unwrap().expect("row present");
        assert!(loaded.hidden, "save must preserve hidden");
        assert!(loaded.pinned, "save must preserve pinned");
        assert!(loaded.archived, "save must preserve archived");
    }
}
