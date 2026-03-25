use crate::{CostRecord, CostStore, CostSummary, TimeRange};

/// Handles recording and aggregation of cost data.
///
/// `CostTracker` is a thin coordination layer over a `CostStore`.
/// It does **not** make limit decisions — that responsibility belongs to `CostGuard`.
pub struct CostTracker {
    store: Box<dyn CostStore>,
}

impl CostTracker {
    pub fn new(store: Box<dyn CostStore>) -> Self {
        Self { store }
    }

    /// Persist a cost record.
    pub async fn record(&self, record: &CostRecord) -> aura_core::Result<()> {
        self.store.record(record).await
    }

    /// Return the sum of `cost_usd` for a user within the given time range.
    pub async fn sum_user(&self, user_id: &str, range: TimeRange) -> aura_core::Result<f64> {
        self.store.sum_user(user_id, range).await
    }

    /// Return an aggregated summary of all records within the given time range.
    pub async fn query_global(&self, range: TimeRange) -> aura_core::Result<CostSummary> {
        self.store.query_global(range).await
    }
}
