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
    metrics: Arc<CostSubscriberMetrics>,
}

/// Cumulative metrics for `CostSubscriber`. Exposed so operators (or
/// the gateway's status endpoint) can detect silent under-billing —
/// a non-zero `lagged_events` count means the broadcast bus dropped
/// LlmSpanEnded events before this subscriber could process them, so
/// `cost_records` and the `CostGuard` quota check are undercounting.
///
/// Clone the `Arc` to read the counters from another task; the
/// internal counters are `AtomicU64` so reads are lock-free.
#[derive(Default)]
pub struct CostSubscriberMetrics {
    /// Number of `LlmSpanEnded` events the broadcast bus dropped
    /// before this subscriber could pick them up. Each lagged event
    /// is one missing `cost_records` row.
    pub lagged_events: std::sync::atomic::AtomicU64,
    /// Total `cost_records` rows successfully written, including
    /// system-driven events with empty `user_id` (e.g. internal
    /// probes). Use [`recorded_user_events`] for user-billable counts.
    ///
    /// [`recorded_user_events`]: Self::recorded_user_events
    pub recorded_events: std::sync::atomic::AtomicU64,
    /// `cost_records` rows attributable to a real user (non-empty
    /// `user_id`) — i.e. the subset that also bumps the monthly cache
    /// and counts toward `CostGuard` quota checks. Operators dashboards
    /// should usually display this counter, not `recorded_events`,
    /// because the difference is system traffic with no billing impact.
    pub recorded_user_events: std::sync::atomic::AtomicU64,
}

impl CostSubscriberMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn lagged(&self) -> u64 {
        self.lagged_events
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn recorded(&self) -> u64 {
        self.recorded_events
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn recorded_user(&self) -> u64 {
        self.recorded_user_events
            .load(std::sync::atomic::Ordering::Relaxed)
    }
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
        Self::with_metrics(store, pricing, CostSubscriberMetrics::new())
    }

    pub fn with_metrics(
        store: Arc<dyn CostStore>,
        pricing: Arc<HashMap<String, ModelPricing>>,
        metrics: Arc<CostSubscriberMetrics>,
    ) -> Self {
        Self {
            store,
            pricing,
            metrics,
        }
    }

    /// Clone the metrics handle so other tasks can read the counters.
    pub fn metrics(&self) -> Arc<CostSubscriberMetrics> {
        Arc::clone(&self.metrics)
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
        let metrics = self.metrics;
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
                        metrics
                            .recorded_events
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Skip the monthly cache bump for system-driven
                        // events that lack a real user (e.g. an internal
                        // probe call); otherwise every such event would
                        // land on a single ("", month) row that conflates
                        // unrelated traffic. The user-billable counter
                        // only ticks past this guard so dashboards don't
                        // conflate probe traffic with real spend.
                        if record.user_id.is_empty() {
                            continue;
                        }
                        metrics
                            .recorded_user_events
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                        // Lagged events are LlmSpanEnded events the bus
                        // dropped before we picked them up — each one
                        // is a missing cost_record. Surface the count so
                        // operators see silent under-billing in metrics
                        // / status endpoints. A non-zero counter here
                        // means CostGuard quota checks are undercounting.
                        metrics
                            .lagged_events
                            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
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

    #[tokio::test]
    async fn metrics_count_recorded_events_through_real_stream() {
        use aura_model::{JobId, SessionId, SpanId};
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let metrics = CostSubscriberMetrics::new();
        let sub = CostSubscriber::with_metrics(
            Arc::clone(&store),
            pricing("m1", 3.0, 15.0),
            Arc::clone(&metrics),
        );
        let stream = TraceEventStream::new();
        let handle = sub.spawn(&stream);

        for _ in 0..3 {
            stream.publish(TraceEvent::LlmSpanEnded {
                span_id: SpanId::new(),
                job_id: JobId::new(),
                session_id: SessionId::from("s1"),
                user_id: "u1".into(),
                model_id: "m1".into(),
                provider: "anth".into(),
                input_tokens: 100,
                output_tokens: 200,
            });
        }
        // Give the spawned task a tick to drain.
        for _ in 0..10 {
            if metrics.recorded() == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(metrics.recorded(), 3, "all three events should land");
        assert_eq!(metrics.lagged(), 0, "no lag in this test path");
        drop(stream);
        // Clean up: subscriber exits when the last sender drops.
        let _ = handle.await;
    }
}
