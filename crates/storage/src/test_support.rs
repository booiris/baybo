//! In-memory store implementations for downstream crates' tests.
//!
//! Gated behind the `test-support` cargo feature so they never ship in
//! release builds. Add new fakes here as the trait surface grows; keep
//! each fake colocated with the trait it implements (in this crate's
//! sibling modules) so changing the trait forces an update.

use std::collections::HashMap;
use std::io::Cursor;

use async_trait::async_trait;
use aura_job::{Job, JobStatus, JobTransition};
use aura_model::{BlobRef, MemoryEntry};
use aura_trace::{SessionTrace, TraceFilter, TraceNode, TraceNodeId};
use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::blob::{
    BlobMeta, BlobReader, BlobStore, ByteStream, Result as BlobResult, SHA256_PREFIX,
};
use crate::cost::{CostRecord, CostResult, CostStore, CostSummary, TimeRange};
use crate::job::{JobStore, Result as JobStoreResult};
use crate::memory::{MemoryStore, Result as MemoryStoreResult};
use crate::secret::{Result as SecretResult, SecretStore};
use crate::trace::{Result as TraceStoreResult, TraceStore};

/// In-memory `SecretStore` for tests. Stores raw `(name, encrypted_value)`
/// pairs in a `Mutex<HashMap>`. No encryption performed here — the bytes
/// are whatever the caller hands in (typically already AES-GCM ciphertext
/// from `SecretVault`).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    data: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live entries. Useful for asserting deterministic-mint
    /// invariants ("same secret minted twice → vault holds one entry").
    pub fn len(&self) -> usize {
        self.data.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> SecretResult<()> {
        self.data
            .lock()
            .insert(name.to_owned(), encrypted_value.to_vec());
        Ok(())
    }

    async fn retrieve(&self, name: &str) -> SecretResult<Option<Vec<u8>>> {
        Ok(self.data.lock().get(name).cloned())
    }

    async fn list(&self) -> SecretResult<Vec<String>> {
        Ok(self.data.lock().keys().cloned().collect())
    }

    async fn delete(&self, name: &str) -> SecretResult<()> {
        self.data.lock().remove(name);
        Ok(())
    }
}

/// In-memory `JobStore` for tests. Keyed by `job.id`. `record_transition`
/// appends to a per-job vector so the order of transitions is preserved.
#[derive(Debug, Default)]
pub struct MemoryJobStore {
    jobs: Mutex<HashMap<String, Job>>,
    transitions: Mutex<HashMap<String, Vec<JobTransition>>>,
}

impl MemoryJobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.jobs.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl JobStore for MemoryJobStore {
    async fn create(&self, job: &Job) -> JobStoreResult<()> {
        self.jobs.lock().insert(job.id.clone(), job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &str) -> JobStoreResult<Option<Job>> {
        Ok(self.jobs.lock().get(job_id).cloned())
    }

    async fn save(&self, job: &Job) -> JobStoreResult<()> {
        self.jobs.lock().insert(job.id.clone(), job.clone());
        Ok(())
    }

    async fn list_by_session(&self, session_id: &str) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_by_status(&self, status: JobStatus) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status == status)
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_job_id: &str) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> JobStoreResult<Vec<Job>> {
        Ok(self.jobs.lock().values().cloned().collect())
    }

    async fn record_transition(&self, transition: &JobTransition) -> JobStoreResult<()> {
        self.transitions
            .lock()
            .entry(transition.job_id.clone())
            .or_default()
            .push(transition.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &str) -> JobStoreResult<Vec<JobTransition>> {
        Ok(self
            .transitions
            .lock()
            .get(job_id)
            .cloned()
            .unwrap_or_default())
    }
}

/// In-memory `CostStore` for tests. Records are appended in arrival
/// order; queries scan linearly. Plenty fast for tests.
#[derive(Debug, Default)]
pub struct MemoryCostStore {
    records: Mutex<Vec<CostRecord>>,
}

impl MemoryCostStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn in_range(record: &CostRecord, range: &TimeRange) -> bool {
    record.timestamp >= range.from && record.timestamp < range.to
}

#[async_trait]
impl CostStore for MemoryCostStore {
    async fn record(&self, record: &CostRecord) -> CostResult<()> {
        self.records.lock().push(record.clone());
        Ok(())
    }

    async fn query_user(&self, user_id: &str, range: TimeRange) -> CostResult<Vec<CostRecord>> {
        Ok(self
            .records
            .lock()
            .iter()
            .filter(|r| r.user_id == user_id && in_range(r, &range))
            .cloned()
            .collect())
    }

    async fn query_global(&self, range: TimeRange) -> CostResult<CostSummary> {
        let mut summary = CostSummary::default();
        for r in self.records.lock().iter().filter(|r| in_range(r, &range)) {
            summary.total_cost_usd += r.cost_usd;
            summary.total_input_tokens += r.input_tokens;
            summary.total_output_tokens += r.output_tokens;
            summary.record_count += 1;
        }
        Ok(summary)
    }

    async fn sum_user(&self, user_id: &str, range: TimeRange) -> CostResult<f64> {
        Ok(self
            .records
            .lock()
            .iter()
            .filter(|r| r.user_id == user_id && in_range(r, &range))
            .map(|r| r.cost_usd)
            .sum())
    }
}

/// In-memory `TraceStore` for tests. Keyed by `session_id`; `save_trace`
/// overwrites prior state for that session (mirrors libsql upsert
/// semantics from the assembly layer's perspective).
#[derive(Debug, Default)]
pub struct MemoryTraceStore {
    traces: Mutex<HashMap<String, SessionTrace>>,
}

impl MemoryTraceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.traces.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TraceStore for MemoryTraceStore {
    async fn save_trace(&self, trace: &SessionTrace) -> TraceStoreResult<()> {
        self.traces
            .lock()
            .insert(trace.session_id.clone(), trace.clone());
        Ok(())
    }

    async fn load_trace(&self, session_id: &str) -> TraceStoreResult<Option<SessionTrace>> {
        Ok(self.traces.lock().get(session_id).cloned())
    }

    async fn query_traces(&self, filter: TraceFilter) -> TraceStoreResult<Vec<SessionTrace>> {
        let traces = self.traces.lock();
        Ok(traces
            .values()
            .filter(|t| {
                filter
                    .session_id
                    .as_deref()
                    .is_none_or(|sid| t.session_id == sid)
            })
            .cloned()
            .collect())
    }

    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> TraceStoreResult<Option<TraceNode>> {
        Ok(self
            .traces
            .lock()
            .get(session_id)
            .and_then(|t| t.nodes.get(node_id).cloned()))
    }
}

/// In-memory `MemoryStore` for tests. Keyed by `entry.id`. Search is a
/// case-insensitive substring match against `key + value` — good enough
/// for asserting "the entry I just stored shows up in search".
#[derive(Debug, Default)]
pub struct MemoryMemoryStore {
    entries: Mutex<HashMap<String, MemoryEntry>>,
}

impl MemoryMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl MemoryStore for MemoryMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> MemoryStoreResult<()> {
        self.entries.lock().insert(entry.id.clone(), entry.clone());
        Ok(())
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> MemoryStoreResult<Option<MemoryEntry>> {
        Ok(self
            .entries
            .lock()
            .values()
            .find(|e| e.user_id == user_id && e.id == key)
            .cloned())
    }

    async fn search(
        &self,
        user_id: &str,
        query: &str,
        limit: usize,
    ) -> MemoryStoreResult<Vec<MemoryEntry>> {
        let q = query.to_ascii_lowercase();
        Ok(self
            .entries
            .lock()
            .values()
            .filter(|e| e.user_id == user_id && e.content.to_ascii_lowercase().contains(&q))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn delete(&self, id: &str) -> MemoryStoreResult<()> {
        self.entries.lock().remove(id);
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> MemoryStoreResult<Vec<MemoryEntry>> {
        Ok(self
            .entries
            .lock()
            .values()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> MemoryStoreResult<Vec<MemoryEntry>> {
        Ok(self.entries.lock().values().cloned().collect())
    }

    async fn get_by_id(&self, id: &str) -> MemoryStoreResult<Option<MemoryEntry>> {
        Ok(self.entries.lock().get(id).cloned())
    }
}

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
    deleted_at: Option<i64>,
}

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.blobs
            .lock()
            .values()
            .filter(|b| b.deleted_at.is_none())
            .count()
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
        let now = chrono::Utc::now().timestamp();
        let mut guard = self.blobs.lock();
        guard
            .entry(blob_id.clone())
            .and_modify(|b| {
                b.mime_type = mime_type.to_owned();
                b.last_accessed_at = now;
                b.deleted_at = None;
            })
            .or_insert(MemoryBlob {
                bytes: bytes.to_vec(),
                mime_type: mime_type.to_owned(),
                created_at: now,
                last_accessed_at: now,
                read_token: read_token.to_owned(),
                deleted_at: None,
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
            Some(b) if b.deleted_at.is_none() => Ok(b.bytes.clone()),
            _ => Err(StorageError::NotFound(format!("blob {blob_id}"))),
        }
    }

    async fn open(&self, blob_id: &str) -> BlobResult<BlobReader> {
        let bytes = self.get(blob_id).await?;
        Ok(Box::pin(Cursor::new(bytes)))
    }

    async fn stat(&self, blob_id: &str) -> BlobResult<BlobMeta> {
        let (_hex, token) = split_id(blob_id)?;
        let now = chrono::Utc::now().timestamp();
        let mut guard = self.blobs.lock();
        match guard.get_mut(blob_id) {
            Some(b) if b.deleted_at.is_none() => {
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
            _ => Err(StorageError::NotFound(format!("blob {blob_id}"))),
        }
    }

    async fn delete(&self, blob_id: &str) -> BlobResult<()> {
        let _ = split_id(blob_id)?;
        if let Some(b) = self.blobs.lock().get_mut(blob_id) {
            b.deleted_at
                .get_or_insert_with(|| chrono::Utc::now().timestamp());
        }
        Ok(())
    }

    async fn purge_older_than(&self, cutoff_unix: i64) -> BlobResult<u64> {
        let mut guard = self.blobs.lock();
        let now = chrono::Utc::now().timestamp();
        let mut purged: u64 = 0;
        for blob in guard.values_mut() {
            if blob.deleted_at.is_none() && blob.last_accessed_at < cutoff_unix {
                blob.deleted_at = Some(now);
                purged += 1;
            }
        }
        Ok(purged)
    }
}

fn split_id(blob_id: &str) -> BlobResult<(&str, &str)> {
    let hex_all = blob_id
        .strip_prefix(SHA256_PREFIX)
        .ok_or_else(|| StorageError::NotFound(format!("invalid blob_id {blob_id}")))?;
    let (hex, token) = hex_all.split_once('.').unwrap_or((hex_all, ""));
    Ok((hex, token))
}
