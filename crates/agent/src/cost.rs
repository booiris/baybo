use std::collections::HashMap;
use std::sync::Arc;

use aura_llm::ModelPricing;
use aura_storage::{CostRecord, CostStore, TimeRange};
use chrono::{Datelike, NaiveDate, Utc};
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::trace::{TraceEvent, TraceEventStream};

#[derive(Debug, Error)]
pub enum CostGuardError {
    #[error("user '{user_id}' exceeded spending limit: current ${current:.4} >= limit ${limit:.4}")]
    UserLimitExceeded {
        user_id: String,
        current: f64,
        limit: f64,
    },

    #[error("global spending limit exceeded: current ${current:.4} >= limit ${limit:.4}")]
    GlobalLimitExceeded { current: f64, limit: f64 },

    #[error("cost query failed: {0}")]
    QueryFailed(#[from] aura_storage::CostError),
}

/// Per-user and global spending limits.
#[derive(Debug, Clone, Default)]
pub struct SpendingLimits {
    /// Maximum USD a single user may spend per day. `None` means unlimited.
    pub user_daily_usd: Option<f64>,
    /// Maximum USD a single user may spend per month. `None` means unlimited.
    pub user_monthly_usd: Option<f64>,
    /// Maximum USD globally per day. `None` means unlimited.
    pub global_daily_usd: Option<f64>,
}

/// Enforces per-user and global spending limits before execution.
///
/// `CostGuard` queries the `CostStore` for current spending totals and
/// rejects requests that would exceed configured limits. It is checked
/// by the `Router` before a message enters an actor.
pub struct CostGuard {
    store: Arc<dyn CostStore>,
    limits: SpendingLimits,
}

impl CostGuard {
    pub fn new(store: Arc<dyn CostStore>, limits: SpendingLimits) -> Self {
        Self { store, limits }
    }

    /// Check whether a user is within all spending limits.
    ///
    /// Returns `Ok(())` if the user may proceed, or `Err(CostGuardError)` if
    /// any limit has been reached.
    pub async fn check_quota(&self, user_id: &str) -> Result<(), CostGuardError> {
        let now = Utc::now();

        // Per-user daily limit
        if let Some(daily_limit) = self.limits.user_daily_usd {
            let day_start = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_local_timezone(Utc).single())
                .unwrap_or(now);
            let range = TimeRange {
                from: day_start,
                to: now,
            };
            let spent = self.store.sum_user(user_id, range).await?;
            if spent >= daily_limit {
                return Err(CostGuardError::UserLimitExceeded {
                    user_id: user_id.to_string(),
                    current: spent,
                    limit: daily_limit,
                });
            }
        }

        // Per-user monthly limit
        if let Some(monthly_limit) = self.limits.user_monthly_usd {
            let naive = now.date_naive();
            let first_of_month =
                NaiveDate::from_ymd_opt(naive.year(), naive.month(), 1).unwrap_or(naive);
            let month_start = first_of_month
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_local_timezone(Utc).single())
                .unwrap_or(now);
            let range = TimeRange {
                from: month_start,
                to: now,
            };
            let spent = self.store.sum_user(user_id, range).await?;
            if spent >= monthly_limit {
                return Err(CostGuardError::UserLimitExceeded {
                    user_id: user_id.to_string(),
                    current: spent,
                    limit: monthly_limit,
                });
            }
        }

        // Global daily limit
        if let Some(global_limit) = self.limits.global_daily_usd {
            let day_start = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_local_timezone(Utc).single())
                .unwrap_or(now);
            let range = TimeRange {
                from: day_start,
                to: now,
            };
            let summary = self.store.query_global(range).await?;
            if summary.total_cost_usd >= global_limit {
                return Err(CostGuardError::GlobalLimitExceeded {
                    current: summary.total_cost_usd,
                    limit: global_limit,
                });
            }
        }

        Ok(())
    }

    pub fn limits(&self) -> &SpendingLimits {
        &self.limits
    }
}

/// Subscribes to a `TraceEventStream` and writes `cost_records` +
/// lazily updates the `user_monthly_cost` cache for every
/// `TraceEvent::LlmSpanEnded` it sees. Designed to run process-wide
/// behind a shared stream — every session's `SpanRecorder` publishes
/// into the same bus, one task drains it.
pub struct CostSubscriber {
    store: Arc<dyn CostStore>,
    pricing: Arc<HashMap<String, ModelPricing>>,
}

fn compute_cost_usd(
    pricing: &HashMap<String, ModelPricing>,
    model_id: &str,
    input_tokens: usize,
    output_tokens: usize,
) -> f64 {
    let Some(p) = pricing.get(model_id) else {
        return 0.0;
    };
    (input_tokens as f64 / 1_000_000.0) * p.input_per_1m_tokens
        + (output_tokens as f64 / 1_000_000.0) * p.output_per_1m_tokens
}

impl CostSubscriber {
    pub fn new(store: Arc<dyn CostStore>, pricing: Arc<HashMap<String, ModelPricing>>) -> Self {
        Self { store, pricing }
    }

    /// Compute USD for the given token counts. Returns 0.0 when the
    /// model is unknown — the raw record still records token counts
    /// so the missing rate can be backfilled later.
    pub fn cost_usd_for(&self, model_id: &str, input_tokens: usize, output_tokens: usize) -> f64 {
        compute_cost_usd(&self.pricing, model_id, input_tokens, output_tokens)
    }

    /// Spawn a tokio task that subscribes to the given stream and
    /// drains forever (until all senders drop). Returns the JoinHandle
    /// so the caller can await shutdown.
    pub fn spawn(self, stream: &TraceEventStream) -> JoinHandle<()> {
        let mut rx = stream.subscribe();
        let store = Arc::clone(&self.store);
        let pricing = self.pricing;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(TraceEvent::LlmSpanEnded {
                        span_id,
                        job_id,
                        session_id,
                        user_id,
                        model_id,
                        input_tokens,
                        output_tokens,
                        ..
                    }) => {
                        let cost_usd =
                            compute_cost_usd(&pricing, &model_id, input_tokens, output_tokens);
                        let now = Utc::now();
                        let record = CostRecord {
                            user_id,
                            session_id,
                            job_id,
                            span_id,
                            model: model_id,
                            input_tokens,
                            output_tokens,
                            cost_usd,
                            timestamp: now,
                            originating_session_deleted_at: None,
                        };
                        if let Err(e) = store.record(&record).await {
                            warn!(error = %e, "failed to write cost_record");
                            continue;
                        }
                        // Skip the monthly cache bump for system-driven
                        // events that lack a real user (e.g. an internal
                        // probe call); otherwise every such event would
                        // land on a single ("", month) row that conflates
                        // unrelated traffic.
                        if record.user_id.is_empty() {
                            continue;
                        }
                        let month = format!("{:04}-{:02}", now.year(), now.month());
                        if let Err(e) = store
                            .bump_user_monthly_cost(&record.user_id, &month, cost_usd)
                            .await
                        {
                            warn!(error = %e, "failed to bump user_monthly_cost cache");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(missed = n, "cost subscriber lagged on TraceEventStream");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        debug!("cost subscriber: stream closed, exiting");
                        return;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_storage::test_support::MemoryCostStore;

    fn pricing(model: &str, input: f64, output: f64) -> Arc<HashMap<String, ModelPricing>> {
        let mut h = HashMap::new();
        h.insert(
            model.to_string(),
            ModelPricing {
                input_per_1m_tokens: input,
                output_per_1m_tokens: output,
            },
        );
        Arc::new(h)
    }

    #[test]
    fn cost_usd_for_known_model() {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let sub = CostSubscriber::new(store, pricing("m1", 3.0, 15.0));
        // 1k input + 2k output → (1000/1e6)*3 + (2000/1e6)*15 = 0.003 + 0.030
        let cost = sub.cost_usd_for("m1", 1_000, 2_000);
        assert!((cost - 0.033).abs() < 1e-9);
    }

    #[test]
    fn cost_usd_for_unknown_model_is_zero() {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let sub = CostSubscriber::new(store, pricing("m1", 3.0, 15.0));
        assert_eq!(sub.cost_usd_for("unknown", 1_000_000, 1_000_000), 0.0);
    }
}
