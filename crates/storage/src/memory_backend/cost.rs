use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use aura_cost::{CostRecord, CostStore, CostSummary, Result, TimeRange};

/// In-memory implementation of [`CostStore`].
pub struct InMemoryCostStore {
    data: Arc<RwLock<Vec<CostRecord>>>,
}

impl InMemoryCostStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl Default for InMemoryCostStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CostStore for InMemoryCostStore {
    async fn record(&self, record: &CostRecord) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.push(record.clone());
        Ok(())
    }

    async fn query_user(&self, user_id: &str, range: TimeRange) -> Result<Vec<CostRecord>> {
        let guard = self.data.read().await;
        let results = guard
            .iter()
            .filter(|r| r.user_id == user_id && r.timestamp >= range.from && r.timestamp < range.to)
            .cloned()
            .collect();
        Ok(results)
    }

    async fn query_global(&self, range: TimeRange) -> Result<CostSummary> {
        let guard = self.data.read().await;
        let mut summary = CostSummary::default();
        for r in guard.iter() {
            if r.timestamp >= range.from && r.timestamp < range.to {
                summary.total_cost_usd += r.cost_usd;
                summary.total_input_tokens += r.input_tokens;
                summary.total_output_tokens += r.output_tokens;
                summary.record_count += 1;
            }
        }
        Ok(summary)
    }

    async fn sum_user(&self, user_id: &str, range: TimeRange) -> Result<f64> {
        let guard = self.data.read().await;
        let total = guard
            .iter()
            .filter(|r| r.user_id == user_id && r.timestamp >= range.from && r.timestamp < range.to)
            .map(|r| r.cost_usd)
            .sum();
        Ok(total)
    }
}
