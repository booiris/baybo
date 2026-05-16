//! Cost management — record LLM-call spend to `CostStore`, enforce
//! spending limits, and surface analytics. Domain types
//! (`CostRecord`, `CostSummary`, `TimeRange`) plus the `CostStore`
//! trait live here alongside the `CostManager` business-logic facade.
//!
//! `aura-storage` provides the libsql implementation of `CostStore`;
//! the trait itself lives here so downstream callers and tests can
//! depend on `aura-cost` alone for cost work.

mod manager;
mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use manager::{CostGuardError, CostManager, CostMetrics, SpendingLimits, cost_call_guard};
pub use store::{CostError, CostRecord, CostResult, CostStore, CostSummary, TimeRange};
