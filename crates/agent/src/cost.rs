use aura_storage::{CostRecord, CostStore};

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
    pub async fn record(&self, record: &CostRecord) -> aura_storage::CostResult<()> {
        self.store.record(record).await
    }
}
