//! Cost management — record LLM-call spend to `CostStore`, enforce
//! spending limits, and surface analytics. This crate owns the
//! `CostManager` business-logic facade; the data types (`CostRecord`,
//! `CostSummary`, `TimeRange`) live in `baybo-model` and the `CostStore`
//! trait lives in `baybo-store` (the ports crate).
//!
//! `baybo-storage` provides the libsql implementation of `CostStore`.

mod error;
mod manager;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use baybo_model::{CostRecord, CostSummary, TimeRange};
pub use baybo_store::CostStore;
pub use error::{CostError, CostResult};
pub use manager::{
    CostGuardError, CostManager, CostMetrics, SpendingLimits, cost_call_guard, cost_hooks,
};
