//! In-memory `CostStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `baybo-cost` (next to the trait it
//! implements) so crates that depend on `baybo-cost` but not on
//! `baybo-storage` can still spin up a fake store for unit tests.

use async_trait::async_trait;
use baybo_model::{SessionId, TurnId};
use parking_lot::Mutex;

use crate::error::CostError;
use baybo_model::{CostRecord, CostSummary, TimeRange};
use baybo_store::cost::{CostGroupBucket, CostGroupKey, CostStore, Result as CostResult};

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

fn fold<'a>(records: impl Iterator<Item = &'a CostRecord>) -> CostSummary {
    let mut summary = CostSummary::default();
    for r in records {
        summary.total_cost_usd += r.cost_usd;
        summary.total_input_tokens += r.input_tokens;
        summary.total_output_tokens += r.output_tokens;
        summary.total_cached_input_tokens += r.cached_input_tokens;
        summary.total_cache_creation_input_tokens += r.cache_creation_input_tokens;
        summary.record_count += 1;
    }
    summary
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

    async fn query_user_summary(&self, user_id: &str, range: TimeRange) -> CostResult<CostSummary> {
        Ok(fold(
            self.records
                .lock()
                .iter()
                .filter(|r| r.user_id == user_id && in_range(r, &range)),
        ))
    }

    async fn query_global(&self, range: TimeRange) -> CostResult<CostSummary> {
        Ok(fold(
            self.records.lock().iter().filter(|r| in_range(r, &range)),
        ))
    }

    async fn query_range_grouped(
        &self,
        range: TimeRange,
        key: CostGroupKey,
    ) -> CostResult<Vec<CostGroupBucket>> {
        let records = self.records.lock();
        let mut by_key: std::collections::BTreeMap<String, Vec<&CostRecord>> = Default::default();
        for r in records.iter().filter(|r| in_range(r, &range)) {
            let k = match key {
                CostGroupKey::Day => r.timestamp.date_naive().format("%Y-%m-%d").to_string(),
                CostGroupKey::Model => r.model.clone(),
                CostGroupKey::Reason => r.reason.to_token().into_owned(),
            };
            by_key.entry(k).or_default().push(r);
        }
        Ok(by_key
            .into_iter()
            .map(|(key, rs)| CostGroupBucket {
                key,
                summary: fold(rs.into_iter()),
            })
            .collect())
    }

    async fn query_session_by_turn(
        &self,
        session_id: &SessionId,
    ) -> CostResult<Vec<CostGroupBucket>> {
        let records = self.records.lock();
        let mut by_turn: std::collections::BTreeMap<String, Vec<&CostRecord>> = Default::default();
        for r in records.iter().filter(|r| &r.session_id == session_id) {
            by_turn.entry(r.turn_id.to_string()).or_default().push(r);
        }
        Ok(by_turn
            .into_iter()
            .map(|(key, rs)| CostGroupBucket {
                key,
                summary: fold(rs.into_iter()),
            })
            .collect())
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
        Ok(fold(
            self.records
                .lock()
                .iter()
                .filter(|r| &r.session_id == session_id),
        ))
    }

    async fn query_turn(&self, turn_id: &TurnId) -> CostResult<CostSummary> {
        Ok(fold(
            self.records.lock().iter().filter(|r| &r.turn_id == turn_id),
        ))
    }
}
