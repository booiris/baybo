//! In-memory store implementations for downstream crates' tests.
//!
//! Gated behind the `test-support` cargo feature so they never ship in
//! release builds. Add new fakes here as the trait surface grows; keep
//! each fake colocated with the trait it implements (in this crate's
//! sibling modules) so changing the trait forces an update.

use std::collections::HashMap;
use std::io::Cursor;

use async_trait::async_trait;
use aura_model::{BlobRef, ChatMessage, Session, SessionId};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::blob::{
    BlobMeta, BlobReader, BlobStore, ByteStream, Result as BlobResult, SHA256_PREFIX,
};
use crate::session::{Result as SessionStoreResult, SessionStore, StoredMessage};
use crate::session_summary::{
    Result as SessionSummaryResult, SessionSummaryRow, SessionSummaryStore,
};

/// In-memory `BlobStore` for tests. Bytes live in a `Mutex<HashMap>`,
/// keyed by the same `sha256:<hex>` blob id the libsql backend uses, so
/// downstream tests can swap between fakes and real stores without
/// changing assertion strings.
#[derive(Debug, Default)]
pub struct MemoryBlobStore {
    blobs: Mutex<HashMap<String, MemoryBlob>>,
}

#[derive(Debug, Clone)]
struct MemoryBlob {
    bytes: Vec<u8>,
    mime_type: String,
    created_at: i64,
    last_accessed_at: i64,
    read_token: String,
}

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.blobs.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    async fn put(
        &self,
        bytes: &[u8],
        mime_type: &str,
        _uploader_identity: Option<&str>,
    ) -> BlobResult<BlobRef> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hex = hex_encode(&hasher.finalize());
        let read_token = "fake-token";
        let blob_id = format!("{SHA256_PREFIX}{hex}.{read_token}");
        let now = chrono::Utc::now().timestamp_micros();
        let mut guard = self.blobs.lock();
        guard
            .entry(blob_id.clone())
            .and_modify(|b| {
                b.mime_type = mime_type.to_owned();
                b.last_accessed_at = now;
            })
            .or_insert(MemoryBlob {
                bytes: bytes.to_vec(),
                mime_type: mime_type.to_owned(),
                created_at: now,
                last_accessed_at: now,
                read_token: read_token.to_owned(),
            });
        Ok(BlobRef { blob_id })
    }

    async fn put_stream(
        &self,
        mut stream: ByteStream,
        mime_type: &str,
        uploader_identity: Option<&str>,
        max_bytes: u64,
    ) -> BlobResult<BlobRef> {
        // Tests only — buffer everything into a Vec and reuse the
        // single-shot `put` path. The real LibsqlBlobStore streams to
        // disk; the fake stays in memory.
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| StorageError::Internal(anyhow::anyhow!("stream io: {e}")))?;
            if (buf.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
                return Err(StorageError::TooLarge {
                    limit: max_bytes,
                    actual: (buf.len() + chunk.len()) as u64,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        self.put(&buf, mime_type, uploader_identity).await
    }

    async fn get(&self, blob_id: &str) -> BlobResult<Vec<u8>> {
        // Funnel through `stat` so the LRU touch + token check fire on
        // every read, matching the libsql backend's contract.
        let _ = self.stat(blob_id).await?;
        match self.blobs.lock().get(blob_id) {
            Some(b) => Ok(b.bytes.clone()),
            None => Err(StorageError::NotFound(format!("blob {blob_id}"))),
        }
    }

    async fn open(&self, blob_id: &str) -> BlobResult<BlobReader> {
        let bytes = self.get(blob_id).await?;
        Ok(Box::pin(Cursor::new(bytes)))
    }

    async fn stat(&self, blob_id: &str) -> BlobResult<BlobMeta> {
        let (_hex, token) = split_id(blob_id)?;
        let now = chrono::Utc::now().timestamp_micros();
        let mut guard = self.blobs.lock();
        match guard.get_mut(blob_id) {
            Some(b) => {
                if b.read_token != token {
                    return Err(StorageError::NotFound(format!("blob {blob_id}")));
                }
                // LRU touch — mirror the libsql backend so tests that
                // exercise touch-on-access behave identically against
                // either store.
                b.last_accessed_at = now;
                Ok(BlobMeta {
                    blob_id: blob_id.to_owned(),
                    mime_type: b.mime_type.clone(),
                    size: b.bytes.len() as u64,
                    created_at: b.created_at,
                    read_token: Some(b.read_token.clone()),
                })
            }
            None => Err(StorageError::NotFound(format!("blob {blob_id}"))),
        }
    }

    async fn delete(&self, blob_id: &str) -> BlobResult<()> {
        let _ = split_id(blob_id)?;
        self.blobs.lock().remove(blob_id);
        Ok(())
    }

    async fn purge_older_than(&self, cutoff_unix: i64) -> BlobResult<u64> {
        let mut guard = self.blobs.lock();
        let before = guard.len();
        guard.retain(|_, blob| blob.last_accessed_at >= cutoff_unix);
        Ok((before - guard.len()) as u64)
    }
}

fn split_id(blob_id: &str) -> BlobResult<(&str, &str)> {
    let hex_all = blob_id
        .strip_prefix(SHA256_PREFIX)
        .ok_or_else(|| StorageError::NotFound(format!("invalid blob_id {blob_id}")))?;
    let (hex, token) = hex_all.split_once('.').unwrap_or((hex_all, ""));
    Ok((hex, token))
}

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

/// In-memory `SessionStore` for tests across the workspace. Lineage /
/// fork columns are stubbed (`list_live_forks` /
/// `list_lineage_children` / `list_active_maintenance_for_parent` /
/// `list_all_maintenance_sessions` all return empty) — tests that
/// need that surface should use the real libsql store via
/// `aura_storage::Store::open` against a tempfile.
#[derive(Default)]
pub struct MemorySessionStore {
    data: Mutex<HashMap<SessionId, Session>>,
    transcripts: Mutex<HashMap<SessionId, Vec<StoredMessageRow>>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn get(&self, session_id: &SessionId) -> SessionStoreResult<Option<Session>> {
        Ok(self.data.lock().get(session_id).cloned())
    }

    async fn save(&self, session: &Session) -> SessionStoreResult<()> {
        self.data.lock().insert(session.id.clone(), session.clone());
        Ok(())
    }

    async fn set_hidden(&self, session_id: &SessionId, hidden: bool) -> SessionStoreResult<bool> {
        let mut data = self.data.lock();
        match data.get_mut(session_id) {
            Some(s) => {
                s.hidden = hidden;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn delete(&self, session_id: &SessionId) -> SessionStoreResult<bool> {
        self.transcripts.lock().remove(session_id);
        Ok(self.data.lock().remove(session_id).is_some())
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> SessionStoreResult<Vec<SessionId>> {
        Ok(self
            .data
            .lock()
            .values()
            .filter(|s| s.last_active < before)
            .map(|s| s.id.clone())
            .collect())
    }

    async fn list_all(&self) -> SessionStoreResult<Vec<Session>> {
        Ok(self.data.lock().values().cloned().collect())
    }

    async fn list_live_forks(
        &self,
        _source_session_id: &SessionId,
    ) -> SessionStoreResult<Vec<SessionId>> {
        Ok(Vec::new())
    }

    async fn list_lineage_children(
        &self,
        _parent_session_id: &SessionId,
    ) -> SessionStoreResult<Vec<(SessionId, aura_model::LineageKind)>> {
        Ok(Vec::new())
    }

    async fn list_active_maintenance_for_parent(
        &self,
        _parent_session_id: &SessionId,
    ) -> SessionStoreResult<Vec<SessionId>> {
        Ok(Vec::new())
    }

    async fn list_all_maintenance_sessions(&self) -> SessionStoreResult<Vec<SessionId>> {
        Ok(Vec::new())
    }

    /// In-memory mock has no `jobs` table, so it cannot do the
    /// cross-table check the libsql backend performs. Conservative:
    /// reuse `list_all_maintenance_sessions` semantics (treat every
    /// maintenance session as unfinished). Tests that need to assert
    /// the "completed pass survives reap" path must run against the
    /// libsql in-memory backend, where the SQL implementation
    /// genuinely joins `jobs.status_kind`.
    async fn list_unfinished_maintenance_sessions(&self) -> SessionStoreResult<Vec<SessionId>> {
        self.list_all_maintenance_sessions().await
    }

    async fn append_session_message(
        &self,
        session_id: &SessionId,
        message: &ChatMessage,
    ) -> SessionStoreResult<i64> {
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

    async fn apply_session_compaction(
        &self,
        session_id: &SessionId,
        new_active: &[ChatMessage],
    ) -> SessionStoreResult<()> {
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
    ) -> SessionStoreResult<Vec<ChatMessage>> {
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

    async fn latest_session_ordinal(
        &self,
        session_id: &SessionId,
    ) -> SessionStoreResult<Option<i64>> {
        Ok(self
            .transcripts
            .lock()
            .get(session_id)
            .and_then(|log| log.iter().map(|m| m.ordinal as i64).max()))
    }

    async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> SessionStoreResult<Vec<StoredMessage>> {
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
    ) -> SessionStoreResult<Option<usize>> {
        Ok(self.transcripts.lock().get(session_id).and_then(|log| {
            let mut active: Vec<&StoredMessageRow> =
                log.iter().filter(|m| m.superseded_by.is_none()).collect();
            active.sort_by_key(|m| m.ordinal);
            active.iter().position(|m| (m.ordinal as i64) == ordinal)
        }))
    }

    async fn count_active_messages(&self, session_id: &SessionId) -> SessionStoreResult<usize> {
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
    ) -> SessionStoreResult<Vec<ChatMessage>> {
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
    ) -> SessionStoreResult<Vec<(i64, ChatMessage)>> {
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
                    .map(|m| (m.ordinal as i64, m.message.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn load_active_session_messages_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        limit: usize,
    ) -> SessionStoreResult<Vec<(i64, ChatMessage)>> {
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
                    .map(|m| (m.ordinal as i64, m.message.clone()))
                    .collect()
            })
            .unwrap_or_default())
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
    async fn get(&self, session_id: &SessionId) -> SessionSummaryResult<Option<SessionSummaryRow>> {
        Ok(self.rows.lock().get(session_id).cloned())
    }

    async fn upsert_success(
        &self,
        session_id: &SessionId,
        cursor: i64,
        cost_micros_delta: i64,
        model_id: &str,
        span_id: &str,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> SessionSummaryResult<()> {
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
                in_flight: false,
                in_flight_owner: None,
            });
        entry.cursor = cursor;
        entry.pass_count += 1;
        entry.cost_micros += cost_micros_delta;
        entry.model_id = model_id.to_string();
        entry.span_id = span_id.to_string();
        entry.updated_at = updated_at;
        entry.error_count = 0;
        entry.in_flight = false;
        entry.in_flight_owner = None;
        Ok(())
    }

    async fn bump_error_count(
        &self,
        session_id: &SessionId,
        model_id: &str,
        span_id: &str,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> SessionSummaryResult<()> {
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
                in_flight: false,
                in_flight_owner: None,
            });
        entry.error_count += 1;
        entry.model_id = model_id.to_string();
        entry.span_id = span_id.to_string();
        entry.updated_at = updated_at;
        entry.in_flight = false;
        entry.in_flight_owner = None;
        Ok(())
    }

    async fn set_in_flight(
        &self,
        session_id: &SessionId,
        in_flight: bool,
        owner: Option<&str>,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> SessionSummaryResult<()> {
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
                in_flight: false,
                in_flight_owner: None,
            });
        entry.in_flight = in_flight;
        entry.in_flight_owner = owner.map(str::to_string);
        entry.updated_at = updated_at;
        Ok(())
    }

    async fn clear_in_flight_if_owned(
        &self,
        session_id: &SessionId,
        owner: &str,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> SessionSummaryResult<bool> {
        let mut guard = self.rows.lock();
        let Some(entry) = guard.get_mut(session_id) else {
            return Ok(false);
        };
        if entry.in_flight_owner.as_deref() != Some(owner) {
            return Ok(false);
        }
        entry.in_flight = false;
        entry.in_flight_owner = None;
        entry.updated_at = updated_at;
        Ok(true)
    }

    async fn clear_all_in_flight(&self) -> SessionSummaryResult<()> {
        let mut guard = self.rows.lock();
        for row in guard.values_mut() {
            row.in_flight = false;
            row.in_flight_owner = None;
        }
        Ok(())
    }

    async fn delete(&self, session_id: &SessionId) -> SessionSummaryResult<bool> {
        Ok(self.rows.lock().remove(session_id).is_some())
    }

    async fn list_session_ids(&self) -> SessionSummaryResult<Vec<SessionId>> {
        Ok(self.rows.lock().keys().cloned().collect())
    }
}
