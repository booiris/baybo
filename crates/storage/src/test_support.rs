//! In-memory store implementations for downstream crates' tests.
//!
//! Gated behind the `test-support` cargo feature so they never ship in
//! release builds. Add new fakes here as the trait surface grows; keep
//! each fake colocated with the trait it implements (in this crate's
//! sibling modules) so changing the trait forces an update.

use std::collections::HashMap;
use std::io::Cursor;

use async_trait::async_trait;
use aura_job::{Job, JobStatusKind, JobTransition};
use aura_model::{BlobRef, JobId, MemoryEntry, SessionId, SpanId, StepId};
use aura_trace::{LifecycleOutcome, RecoveredSpan, RecoveryReport, Span, SpanEvent, Step};
use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::blob::{
    BlobMeta, BlobReader, BlobStore, ByteStream, Result as BlobResult, SHA256_PREFIX,
};
use crate::cost::{CostRecord, CostResult, CostStore, CostSummary, TimeRange, UserMonthlyCost};
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
    jobs: Mutex<HashMap<JobId, Job>>,
    transitions: Mutex<HashMap<JobId, Vec<JobTransition>>>,
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
        self.jobs.lock().insert(job.id, job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &JobId) -> JobStoreResult<Option<Job>> {
        Ok(self.jobs.lock().get(job_id).cloned())
    }

    async fn save(&self, job: &Job) -> JobStoreResult<()> {
        self.jobs.lock().insert(job.id, job.clone());
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| &j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_by_status_kind(&self, kind: JobStatusKind) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status.kind() == kind)
            .cloned()
            .collect())
    }

    async fn list_recoverable(&self) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.status.kind().needs_recovery())
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_job_id: &JobId) -> JobStoreResult<Vec<Job>> {
        Ok(self
            .jobs
            .lock()
            .values()
            .filter(|j| j.parent_job_id.as_ref() == Some(parent_job_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> JobStoreResult<Vec<Job>> {
        Ok(self.jobs.lock().values().cloned().collect())
    }

    async fn record_transition(&self, transition: &JobTransition) -> JobStoreResult<()> {
        self.transitions
            .lock()
            .entry(transition.job_id)
            .or_default()
            .push(transition.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &JobId) -> JobStoreResult<Vec<JobTransition>> {
        Ok(self
            .transitions
            .lock()
            .get(job_id)
            .cloned()
            .unwrap_or_default())
    }
}

/// In-memory `CostStore` for tests. Records are appended in arrival
/// order; queries scan linearly. Plenty fast for tests. Carries the
/// `user_monthly_cost` cache too so `CostSubscriber` integration
/// tests can read it back.
#[derive(Debug, Default)]
pub struct MemoryCostStore {
    records: Mutex<Vec<CostRecord>>,
    monthly: Mutex<HashMap<(String, String), UserMonthlyCost>>,
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

    async fn query_session(&self, session_id: &SessionId) -> CostResult<CostSummary> {
        let mut summary = CostSummary::default();
        for r in self
            .records
            .lock()
            .iter()
            .filter(|r| &r.session_id == session_id)
        {
            summary.total_cost_usd += r.cost_usd;
            summary.total_input_tokens += r.input_tokens;
            summary.total_output_tokens += r.output_tokens;
            summary.record_count += 1;
        }
        Ok(summary)
    }

    async fn query_job(&self, job_id: &JobId) -> CostResult<CostSummary> {
        let mut summary = CostSummary::default();
        for r in self.records.lock().iter().filter(|r| &r.job_id == job_id) {
            summary.total_cost_usd += r.cost_usd;
            summary.total_input_tokens += r.input_tokens;
            summary.total_output_tokens += r.output_tokens;
            summary.record_count += 1;
        }
        Ok(summary)
    }

    async fn bump_user_monthly_cost(
        &self,
        user_id: &str,
        month: &str,
        delta_usd: f64,
    ) -> CostResult<()> {
        let now = chrono::Utc::now();
        let key = (user_id.to_string(), month.to_string());
        let mut map = self.monthly.lock();
        let entry = map.entry(key).or_insert_with(|| UserMonthlyCost {
            user_id: user_id.to_string(),
            month: month.to_string(),
            cost_usd: 0.0,
            updated_at: now,
        });
        entry.cost_usd += delta_usd;
        entry.updated_at = now;
        Ok(())
    }

    async fn get_user_monthly_cost(
        &self,
        user_id: &str,
        month: &str,
    ) -> CostResult<Option<UserMonthlyCost>> {
        Ok(self
            .monthly
            .lock()
            .get(&(user_id.to_string(), month.to_string()))
            .cloned())
    }

    async fn purge_user_monthly_cost_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> CostResult<u64> {
        let mut map = self.monthly.lock();
        let before = map.len();
        map.retain(|_, v| v.updated_at >= cutoff);
        Ok((before - map.len()) as u64)
    }

    async fn purge_cost_records_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> CostResult<u64> {
        let mut records = self.records.lock();
        let before = records.len();
        records.retain(|r| r.timestamp >= cutoff);
        Ok((before - records.len()) as u64)
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

/// In-memory `TraceStore` for tests. Steps are keyed by `StepId`,
/// spans by `SpanId`, span events by `(SpanId, seq)`. The recovery
/// scan rewrites half-open spans the same way the libsql implementation
/// does (`Cancelled { SystemCrash }` outcome + `ended_at` stamp).
#[derive(Debug, Default)]
pub struct MemoryTraceStore {
    steps: Mutex<HashMap<StepId, Step>>,
    spans: Mutex<HashMap<SpanId, Span>>,
    span_events: Mutex<HashMap<SpanId, Vec<SpanEvent>>>,
}

impl MemoryTraceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.spans.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TraceStore for MemoryTraceStore {
    async fn save_step(&self, step: &Step) -> TraceStoreResult<()> {
        self.steps.lock().insert(step.id, step.clone());
        Ok(())
    }

    async fn load_step(&self, step_id: &StepId) -> TraceStoreResult<Option<Step>> {
        Ok(self.steps.lock().get(step_id).cloned())
    }

    async fn list_steps_by_job(&self, job_id: &JobId) -> TraceStoreResult<Vec<Step>> {
        let mut out: Vec<Step> = self
            .steps
            .lock()
            .values()
            .filter(|s| &s.job_id == job_id)
            .cloned()
            .collect();
        out.sort_by_key(|s| s.started_at);
        Ok(out)
    }

    async fn save_span(&self, span: &Span) -> TraceStoreResult<()> {
        self.spans.lock().insert(span.id, span.clone());
        Ok(())
    }

    async fn load_span(&self, span_id: &SpanId) -> TraceStoreResult<Option<Span>> {
        Ok(self.spans.lock().get(span_id).cloned())
    }

    async fn list_spans_by_step(&self, step_id: &StepId) -> TraceStoreResult<Vec<Span>> {
        let mut out: Vec<Span> = self
            .spans
            .lock()
            .values()
            .filter(|s| &s.step_id == step_id)
            .cloned()
            .collect();
        out.sort_by_key(|s| s.started_at);
        Ok(out)
    }

    async fn append_span_event(&self, event: &SpanEvent) -> TraceStoreResult<()> {
        self.span_events
            .lock()
            .entry(event.span_id)
            .or_default()
            .push(event.clone());
        Ok(())
    }

    async fn list_span_events(&self, span_id: &SpanId) -> TraceStoreResult<Vec<SpanEvent>> {
        Ok(self
            .span_events
            .lock()
            .get(span_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn recover_half_open_spans(&self) -> TraceStoreResult<RecoveryReport> {
        let now = chrono::Utc::now();
        let steps_by_id: HashMap<StepId, JobId> = self
            .steps
            .lock()
            .iter()
            .map(|(k, v)| (*k, v.job_id))
            .collect();
        let mut spans = self.spans.lock();
        let mut recovered = Vec::new();
        for span in spans.values_mut() {
            if span.ended_at.is_none() {
                let job_id = steps_by_id.get(&span.step_id).copied().unwrap_or_default();
                span.ended_at = Some(now);
                span.outcome = LifecycleOutcome::Cancelled {
                    reason: aura_job::CancelReason::SystemCrash,
                };
                recovered.push(RecoveredSpan::for_system_crash(job_id, span.id, now));
            }
        }
        Ok(RecoveryReport::new(recovered))
    }
}

/// In-memory `TraceEventStore` for tests. Append-only WAL log; replay
/// returns events in insertion order; compaction drops events whose
/// `at` is strictly before the cutoff.
#[derive(Debug, Default)]
pub struct MemoryTraceEventStore {
    by_session: Mutex<HashMap<aura_model::SessionId, Vec<crate::trace::TraceLogEvent>>>,
}

impl MemoryTraceEventStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl crate::trace::TraceEventStore for MemoryTraceEventStore {
    async fn append(&self, event: &crate::trace::TraceLogEvent) -> TraceStoreResult<()> {
        self.by_session
            .lock()
            .entry(event.session_id.clone())
            .or_default()
            .push(event.clone());
        Ok(())
    }

    async fn replay_session(
        &self,
        session_id: &aura_model::SessionId,
    ) -> TraceStoreResult<Vec<crate::trace::TraceLogEvent>> {
        Ok(self
            .by_session
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn compact_before(&self, cutoff: chrono::DateTime<chrono::Utc>) -> TraceStoreResult<u64> {
        let mut by_session = self.by_session.lock();
        let mut purged = 0u64;
        for events in by_session.values_mut() {
            let before = events.len();
            events.retain(|e| e.at >= cutoff);
            purged += (before - events.len()) as u64;
        }
        Ok(purged)
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
