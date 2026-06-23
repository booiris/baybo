//! In-memory `CostStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `baybo-cost` (next to the trait it
//! implements) so crates that depend on `baybo-cost` but not on
//! `baybo-storage` can still spin up a fake store for unit tests.

use async_trait::async_trait;
use baybo_model::{JobId, SessionId};
use parking_lot::Mutex;

use crate::error::CostError;
use baybo_model::{CostRecord, CostSummary, TimeRange};
use baybo_store::cost::{CostStore, Result as CostResult};

const fn assert_send<T: Send>() {}
const _: () = assert_send::<CostError>();

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

    /// Snapshot every persisted `CostRecord` in arrival order. Cloned
    /// on read so callers can iterate without holding the mutex.
    pub fn records(&self) -> Vec<CostRecord> {
        self.records.lock().clone()
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
            summary.total_cached_input_tokens += r.cached_input_tokens;
            summary.total_cache_creation_input_tokens += r.cache_creation_input_tokens;
            summary.record_count += 1;
        }
        Ok(summary)
    }

    async fn query_records_in_range(&self, range: TimeRange) -> CostResult<Vec<CostRecord>> {
        let mut out: Vec<CostRecord> = self
            .records
            .lock()
            .iter()
            .filter(|r| in_range(r, &range))
            .cloned()
            .collect();
        out.sort_by_key(|r| r.timestamp);
        Ok(out)
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
            summary.total_cached_input_tokens += r.cached_input_tokens;
            summary.total_cache_creation_input_tokens += r.cache_creation_input_tokens;
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
            summary.total_cached_input_tokens += r.cached_input_tokens;
            summary.total_cache_creation_input_tokens += r.cache_creation_input_tokens;
            summary.record_count += 1;
        }
        Ok(summary)
    }
}
