use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aura_llm::ModelPricing;
use aura_model::{JobId, MicroUsd, SessionId, SpanId};
use chrono::{Datelike, NaiveDate, Utc};
use parking_lot::RwLock;
use thiserror::Error;
use tracing::warn;

use aura_model::{CostRecord, TimeRange};
use aura_store::CostStore;

#[derive(Debug, Error)]
pub enum CostGuardError {
    #[error("daily spending limit exceeded: current {current} >= limit {limit}")]
    DailyLimitExceeded { current: MicroUsd, limit: MicroUsd },

    #[error("monthly spending limit exceeded: current {current} >= limit {limit}")]
    MonthlyLimitExceeded { current: MicroUsd, limit: MicroUsd },
}

/// Total spending caps. This is a personal-assistant deployment so
/// budgeting is one global pool, not per-user — `user_id` on
/// `cost_records` is just provenance, never a budget dimension.
#[derive(Debug, Clone, Default)]
pub struct SpendingLimits {
    /// Cap on total daily spend. `None` means unlimited.
    pub daily_usd: Option<MicroUsd>,
    /// Cap on total monthly spend. `None` means unlimited.
    pub monthly_usd: Option<MicroUsd>,
}

// ── CostManager ──────────────────────────────────────────────────────

/// Single owner of cost-related side effects for the agent loop.
/// `agent_loop::call_llm` invokes [`Self::record_call`] with the
/// token counts it already has:
///
/// 1. The in-memory accumulators update synchronously, so the next
///    iteration's [`Self::check`] sees the spend immediately.
/// 2. A `tokio::spawn`'d task persists the `cost_records` row.
///    Persist failures log a warning and tick
///    `metrics.persist_failures`; they never fail the call.
///
/// `Router` (in `aura-agent`) also calls `check` at message ingress so
/// an over-cap user never even gets an actor spun up. [`Self::hydrate`]
/// runs at boot to rebuild today's / this month's totals from disk so a
/// restart doesn't silently widen the budget.
pub struct CostManager {
    store: Arc<dyn CostStore>,
    /// `RwLock` so the boot-time refresh can overlay rates without
    /// blocking the LLM-client wiring on the network fetch.
    pricing: Arc<RwLock<HashMap<String, ModelPricing>>>,
    /// `RwLock` so config hot-reload can swap the caps live; the next
    /// `check` reads the new values. Swapped via `set_limits`.
    limits: RwLock<SpendingLimits>,
    state: Arc<RwLock<BudgetState>>,
    metrics: Arc<CostMetrics>,
}

/// Lock-free counters for ops dashboards.
#[derive(Default)]
pub struct CostMetrics {
    /// Total `cost_records` rows successfully persisted.
    pub recorded_events: AtomicU64,
    /// Increments when the fire-and-forget persist task fails. A
    /// non-zero value means in-memory budget state is ahead of
    /// `cost_records` until the next successful write reconciles.
    pub persist_failures: AtomicU64,
}

impl CostMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn recorded(&self) -> u64 {
        self.recorded_events.load(Ordering::Relaxed)
    }

    pub fn persist_failures(&self) -> u64 {
        self.persist_failures.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct BudgetState {
    daily: MicroUsd,
    daily_date: Option<NaiveDate>,
    monthly: MicroUsd,
    monthly_year_month: Option<(i32, u32)>,
}

impl BudgetState {
    /// Drop yesterday's daily total / last month's monthly total if
    /// the wall clock has moved forward into a new window.
    fn roll_to(&mut self, today: NaiveDate, year_month: (i32, u32)) {
        if self.daily_date != Some(today) {
            self.daily_date = Some(today);
            self.daily = MicroUsd::ZERO;
        }
        if self.monthly_year_month != Some(year_month) {
            self.monthly_year_month = Some(year_month);
            self.monthly = MicroUsd::ZERO;
        }
    }

    /// Spend in the current daily window. Returns zero when the
    /// in-memory window is stale (the rollover wipe happens lazily
    /// on the next mutating path).
    fn daily_for(&self, today: NaiveDate) -> MicroUsd {
        if self.daily_date == Some(today) {
            self.daily
        } else {
            MicroUsd::ZERO
        }
    }

    fn monthly_for(&self, year_month: (i32, u32)) -> MicroUsd {
        if self.monthly_year_month == Some(year_month) {
            self.monthly
        } else {
            MicroUsd::ZERO
        }
    }
}

/// Bills a single LLM call against the model's per-1M rates.
///
/// Convention: `input_tokens` is the **total** prompt token count
/// (including any cached or cache-write portion). `cached_input_tokens`
/// and `cache_creation_input_tokens` are subsets — billing splits
/// `input_tokens` into three buckets (full-rate, cache-read, cache-write)
/// using `cached_input_per_1m_tokens` / `cache_write_per_1m_tokens`
/// when present, falling back to `input_per_1m_tokens` when the
/// model's pricing row hasn't yet exposed cache rates. The Anthropic
/// adapter (`from_anthropic_*` in `aura-llm`) normalises its native
/// disjoint buckets to this convention so the formula is uniform
/// across providers.
fn compute_cost_usd(
    pricing: &HashMap<String, ModelPricing>,
    model_id: &str,
    input_tokens: usize,
    output_tokens: usize,
    cached_input_tokens: usize,
    cache_creation_input_tokens: usize,
) -> MicroUsd {
    let Some(p) = pricing.get(model_id) else {
        return MicroUsd::ZERO;
    };
    let cached_subset = cached_input_tokens.saturating_add(cache_creation_input_tokens);
    let full_rate_input = input_tokens.saturating_sub(cached_subset) as u64;
    let cached_rate = p
        .cached_input_per_1m_tokens
        .unwrap_or(p.input_per_1m_tokens);
    let cache_write_rate = p.cache_write_per_1m_tokens.unwrap_or(p.input_per_1m_tokens);
    MicroUsd::cost_for_tokens(p.input_per_1m_tokens, full_rate_input)
        + MicroUsd::cost_for_tokens(cached_rate, cached_input_tokens as u64)
        + MicroUsd::cost_for_tokens(cache_write_rate, cache_creation_input_tokens as u64)
        + MicroUsd::cost_for_tokens(p.output_per_1m_tokens, output_tokens as u64)
}

/// Live rate must drift by this fraction or more vs. the bundled
/// snapshot before [`CostManager::merge_pricings`] surfaces a
/// `warn!`. Above the rounding noise that's normal between scrapes
/// (sub-percent on most prices) but below a tier rename or
/// list-price cut.
const PRICING_DRIFT_WARN: f64 = 0.25;

fn drift_ratio(a: aura_model::MicroUsd, b: aura_model::MicroUsd) -> f64 {
    let a = a.into_micros() as f64;
    let b = b.into_micros() as f64;
    if a <= 0.0 {
        return 1.0;
    }
    (a - b).abs() / a
}

impl CostManager {
    pub fn new(
        store: Arc<dyn CostStore>,
        pricing: HashMap<String, ModelPricing>,
        limits: SpendingLimits,
    ) -> Arc<Self> {
        Self::with_metrics(store, pricing, limits, CostMetrics::new())
    }

    pub fn with_metrics(
        store: Arc<dyn CostStore>,
        pricing: HashMap<String, ModelPricing>,
        limits: SpendingLimits,
        metrics: Arc<CostMetrics>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            pricing: Arc::new(RwLock::new(pricing)),
            limits: RwLock::new(limits),
            state: Arc::new(RwLock::new(BudgetState::default())),
            metrics,
        })
    }

    pub fn metrics(&self) -> Arc<CostMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Swap the spending caps live (config hot-reload). The next
    /// [`Self::check`] sees the new caps; an in-flight check finishes
    /// on whichever snapshot it already read.
    pub fn set_limits(&self, limits: SpendingLimits) {
        *self.limits.write() = limits;
    }

    /// Compute spend for the given token counts. Returns
    /// [`MicroUsd::ZERO`] when the model is unknown. See
    /// [`compute_cost_usd`] for the bucket convention.
    pub fn cost_usd_for(
        &self,
        model_id: &str,
        input_tokens: usize,
        output_tokens: usize,
        cached_input_tokens: usize,
        cache_creation_input_tokens: usize,
    ) -> MicroUsd {
        compute_cost_usd(
            &self.pricing.read(),
            model_id,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
        )
    }

    /// Overlay live rates onto the snapshot map; `warn!`s when drift
    /// vs. the existing rate exceeds [`PRICING_DRIFT_WARN`] so a stale
    /// bundle is visible without spamming on rounding noise.
    pub fn merge_pricings(&self, overlay: HashMap<String, ModelPricing>) {
        if overlay.is_empty() {
            return;
        }
        let mut map = self.pricing.write();
        for (model_id, live) in overlay {
            if let Some(prev) = map.get(&model_id) {
                let drift = drift_ratio(prev.input_per_1m_tokens, live.input_per_1m_tokens).max(
                    drift_ratio(prev.output_per_1m_tokens, live.output_per_1m_tokens),
                );
                if drift >= PRICING_DRIFT_WARN {
                    warn!(
                        model = %model_id, drift_ratio = drift,
                        "openrouter pricing drifted from bundled snapshot — \
                         run scripts/regen-openrouter-pricings.sh and commit",
                    );
                }
            }
            map.insert(model_id, live);
        }
    }

    /// Pre-fill in-memory accumulators from persisted `cost_records`
    /// for the current UTC day / month. Hydration failures log+swallow
    /// and leave the affected accumulator empty — operator-set caps
    /// are silently widened until the next `record_call` rebuilds
    /// state, so monitor `CostManager day/month hydrate failed` warns.
    ///
    /// **Must be awaited before any [`Self::record_call`] can fire.**
    /// Hydrate **overwrites** the daily / monthly accumulators with the
    /// SUM read from the store; a `record_call` that lands during the
    /// awaited DB query would have its bump silently dropped.
    /// `runtime.rs` enforces this by awaiting `hydrate` before the
    /// `spawn_actor_for` closure even constructs an actor (no actor →
    /// no `record_call` source). New callers must keep that invariant.
    pub async fn hydrate(self: &Arc<Self>) {
        let now = Utc::now();
        let Some(day_start) = utc_day_start(now) else {
            return;
        };
        let Some(month_start) = utc_month_start(now) else {
            return;
        };
        let today = now.date_naive();
        let year_month = (now.year(), now.month());

        match self
            .store
            .query_records_in_range(TimeRange {
                from: day_start,
                to: now,
            })
            .await
        {
            Ok(records) => {
                let total: MicroUsd = records.iter().map(|r| r.cost_usd).sum();
                let mut state = self.state.write();
                state.daily_date = Some(today);
                state.daily = total;
            }
            Err(e) => warn!(error = %e, "CostManager day hydrate failed"),
        }
        match self
            .store
            .query_records_in_range(TimeRange {
                from: month_start,
                to: now,
            })
            .await
        {
            Ok(records) => {
                let total: MicroUsd = records.iter().map(|r| r.cost_usd).sum();
                let mut state = self.state.write();
                state.monthly_year_month = Some(year_month);
                state.monthly = total;
            }
            Err(e) => warn!(error = %e, "CostManager month hydrate failed"),
        }
    }

    /// Record one finished LLM call and return the cost that was
    /// billed against the budget. Memory first, disk second:
    /// 1. The in-memory budget state updates synchronously so the
    ///    next [`Self::check`] sees the new spend before this method
    ///    returns.
    /// 2. The `cost_records` row is persisted in a `tokio::spawn`'d
    ///    task. Persist errors log+swallow and tick
    ///    `metrics.persist_failures` so dashboards can spot drift
    ///    between in-memory and on-disk totals.
    ///
    /// `user_id` is recorded on the row for provenance only — budget
    /// is global, never per-user. The returned [`MicroUsd`] lets a
    /// caller surface the billed amount (e.g. as `LlmCallResult.cost_micros`)
    /// without recomputing pricing.
    #[allow(clippy::too_many_arguments)]
    pub fn record_call(
        self: &Arc<Self>,
        user_id: &str,
        session_id: SessionId,
        job_id: JobId,
        span_id: SpanId,
        model_id: &str,
        input_tokens: usize,
        output_tokens: usize,
        cached_input_tokens: usize,
        cache_creation_input_tokens: usize,
    ) -> MicroUsd {
        let now = Utc::now();
        let cost = compute_cost_usd(
            &self.pricing.read(),
            model_id,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
        );

        if cost > MicroUsd::ZERO {
            let mut state = self.state.write();
            state.roll_to(now.date_naive(), (now.year(), now.month()));
            state.daily += cost;
            state.monthly += cost;
        }

        let record = CostRecord {
            user_id: user_id.to_string(),
            session_id,
            job_id,
            span_id,
            model: model_id.to_string(),
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            cost_usd: cost,
            timestamp: now,
        };
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.persist(record).await;
        });
        cost
    }

    /// Record token consumption for a subscription-billed external
    /// agent run (claude code Max, codex on ChatGPT). Cost is
    /// **always [`MicroUsd::ZERO`]** — the operator's subscription
    /// covers it, so we never price these tokens and never touch the
    /// daily/monthly budget accumulators. Tokens are still persisted
    /// for observability (the analytics per-model/session breakdowns).
    #[allow(clippy::too_many_arguments)]
    pub fn record_external_tokens(
        self: &Arc<Self>,
        user_id: &str,
        session_id: SessionId,
        job_id: JobId,
        span_id: SpanId,
        model_id: &str,
        input_tokens: usize,
        output_tokens: usize,
        cached_input_tokens: usize,
        cache_creation_input_tokens: usize,
    ) {
        let record = CostRecord {
            user_id: user_id.to_string(),
            session_id,
            job_id,
            span_id,
            model: model_id.to_string(),
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            cost_usd: MicroUsd::ZERO,
            timestamp: Utc::now(),
        };
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.persist(record).await;
        });
    }

    async fn persist(self: Arc<Self>, record: CostRecord) {
        if let Err(e) = self.store.record(&record).await {
            warn!(error = %e, "failed to write cost_record");
            self.metrics
                .persist_failures
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.metrics.recorded_events.fetch_add(1, Ordering::Relaxed);
    }

    /// Synchronous in-memory budget gate. Runs before every
    /// dispatched LLM call (via `GuardedLlm`) and at `Router`
    /// ingress. A UTC rollover between the most recent `record_call`
    /// and `now` is honoured by treating the stale window as zero
    /// spend.
    pub fn check(&self) -> Result<(), CostGuardError> {
        let now = Utc::now();
        let today = now.date_naive();
        let year_month = (now.year(), now.month());
        let (daily_cap, monthly_cap) = {
            let limits = self.limits.read();
            (limits.daily_usd, limits.monthly_usd)
        };
        let state = self.state.read();

        if let Some(daily_limit) = daily_cap {
            let spent = state.daily_for(today);
            if spent >= daily_limit {
                return Err(CostGuardError::DailyLimitExceeded {
                    current: spent,
                    limit: daily_limit,
                });
            }
        }
        if let Some(monthly_limit) = monthly_cap {
            let spent = state.monthly_for(year_month);
            if spent >= monthly_limit {
                return Err(CostGuardError::MonthlyLimitExceeded {
                    current: spent,
                    limit: monthly_limit,
                });
            }
        }
        Ok(())
    }
}

/// Bridge `CostManager` to the `LlmCallGuard` closure shape that
/// `LlmProviderRegistry::create_client` (and other low-level
/// construction sites) take. Lives outside the manager so this crate
/// doesn't need to know about `LlmError` — callers in the agent layer
/// can compose `CostManager::check` with `LlmError::GuardRejected`
/// using [`cost_call_guard`].
pub fn cost_call_guard(manager: &Arc<CostManager>) -> aura_llm::LlmCallGuard {
    let cm = Arc::clone(manager);
    Arc::new(move || {
        cm.check()
            .map_err(|e| aura_llm::LlmError::GuardRejected(e.to_string()))
    })
}

fn utc_day_start(now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_local_timezone(Utc).single())
}

fn utc_month_start(now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    let d = now.date_naive();
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1)?
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_local_timezone(Utc).single())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryCostStore;

    fn pricing(model: &str, input: f64, output: f64) -> HashMap<String, ModelPricing> {
        let mut h = HashMap::new();
        h.insert(
            model.to_string(),
            ModelPricing {
                input_per_1m_tokens: MicroUsd::from_usd_decimal(input),
                output_per_1m_tokens: MicroUsd::from_usd_decimal(output),
                ..Default::default()
            },
        );
        h
    }

    #[test]
    fn cost_usd_for_known_model() {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let sub = CostManager::new(store, pricing("m1", 3.0, 15.0), SpendingLimits::default());
        // 1k input + 2k output → (1000/1e6)*3 + (2000/1e6)*15 = 0.003 + 0.030 = $0.033
        let cost = sub.cost_usd_for("m1", 1_000, 2_000, 0, 0);
        assert_eq!(cost, MicroUsd::from_usd_decimal(0.033));
    }

    #[test]
    fn cost_usd_for_unknown_model_is_zero() {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let sub = CostManager::new(store, pricing("m1", 3.0, 15.0), SpendingLimits::default());
        assert_eq!(
            sub.cost_usd_for("unknown", 1_000_000, 1_000_000, 0, 0),
            MicroUsd::ZERO
        );
    }

    #[test]
    fn merge_pricings_overlays_live_rate_visible_to_cost_usd_for() {
        // The async-spawned boot refresh hands an overlay to
        // `merge_pricings`; the next `cost_usd_for` call must see it.
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let cm = CostManager::new(store, pricing("m1", 3.0, 15.0), SpendingLimits::default());
        // Before merge: original snapshot rate.
        assert_eq!(
            cm.cost_usd_for("m1", 1_000, 0, 0, 0),
            MicroUsd::from_usd_decimal(0.003),
        );
        // Halve the rate live.
        cm.merge_pricings(pricing("m1", 1.5, 7.5));
        assert_eq!(
            cm.cost_usd_for("m1", 1_000, 0, 0, 0),
            MicroUsd::from_usd_decimal(0.0015),
        );
    }

    #[test]
    fn cost_usd_for_splits_cached_subset_at_cached_rate() {
        // input_tokens is the TOTAL prompt; cached_input_tokens is a
        // subset billed at the (cheaper) cached rate. With cached =
        // 600 of 1000 prompt tokens at full=$3 / cached=$0.30:
        //   non-cached portion: 400 tokens × $3/M = $0.0012
        //   cached portion:     600 tokens × $0.30/M = $0.00018
        //   output:               0
        //   total = $0.00138
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let mut h = pricing("m1", 3.0, 15.0);
        if let Some(p) = h.get_mut("m1") {
            p.cached_input_per_1m_tokens = Some(MicroUsd::from_usd_decimal(0.30));
        }
        let cm = CostManager::new(store, h, SpendingLimits::default());
        assert_eq!(
            cm.cost_usd_for("m1", 1_000, 0, 600, 0),
            MicroUsd::from_usd_decimal(0.00138),
        );
    }

    #[test]
    fn cost_usd_for_bills_cache_creation_at_write_rate() {
        // Anthropic-style: cache_creation portion charged at 1.25× input.
        // input=1000 (total), cache_creation=200, full=$3, write=$3.75:
        //   non-cached: 800 × $3/M = $0.0024
        //   write:      200 × $3.75/M = $0.00075
        //   total = $0.00315
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let mut h = pricing("m1", 3.0, 15.0);
        if let Some(p) = h.get_mut("m1") {
            p.cache_write_per_1m_tokens = Some(MicroUsd::from_usd_decimal(3.75));
        }
        let cm = CostManager::new(store, h, SpendingLimits::default());
        assert_eq!(
            cm.cost_usd_for("m1", 1_000, 0, 0, 200),
            MicroUsd::from_usd_decimal(0.00315),
        );
    }

    #[test]
    fn cost_usd_for_falls_back_to_input_rate_when_cache_rates_missing() {
        // Pricing row without cache rates → cache buckets bill at the
        // standard input rate. Total cost equals input_tokens × input_rate
        // regardless of how it splits across buckets — pin that
        // invariant so a future refactor doesn't accidentally drop the
        // cached portion to $0.
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let cm = CostManager::new(store, pricing("m1", 3.0, 15.0), SpendingLimits::default());
        let with_cache = cm.cost_usd_for("m1", 1_000, 0, 700, 100);
        let without_cache = cm.cost_usd_for("m1", 1_000, 0, 0, 0);
        assert_eq!(with_cache, without_cache);
    }

    #[test]
    fn merge_pricings_inserts_new_models_without_clobbering_existing() {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let cm = CostManager::new(store, pricing("m1", 3.0, 15.0), SpendingLimits::default());
        cm.merge_pricings(pricing("m2", 0.5, 2.0));
        // Original m1 still priced at the snapshot rate.
        assert_eq!(
            cm.cost_usd_for("m1", 1_000, 0, 0, 0),
            MicroUsd::from_usd_decimal(0.003),
        );
        // New m2 attribution lands at the merged rate.
        assert_eq!(
            cm.cost_usd_for("m2", 1_000_000, 0, 0, 0),
            MicroUsd::from_usd_decimal(0.5),
        );
    }

    #[test]
    fn drift_ratio_clamps_zero_baseline_and_handles_typical_deltas() {
        // Identical → no drift.
        let p = MicroUsd::from_micros(3_000_000);
        assert_eq!(drift_ratio(p, p), 0.0);
        // 25% delta → 0.25 ratio (the default warn threshold).
        assert!(
            (drift_ratio(
                MicroUsd::from_micros(4_000_000),
                MicroUsd::from_micros(3_000_000)
            ) - 0.25)
                .abs()
                < 1e-9,
        );
        // Zero baseline doesn't divide-by-zero — returns 1.0 so the warn
        // surface fires loudly on the first real rate after a misconfigured
        // snapshot rather than silently treating it as no-drift.
        assert_eq!(drift_ratio(MicroUsd::ZERO, MicroUsd::from_micros(1)), 1.0);
    }

    /// Push a synthetic spend row for `user_id` with `cost`. Timestamp
    /// is `Utc::now()` so the calendar-day / calendar-month windows in
    /// `check_quota` always include it.
    async fn record_spend(store: &Arc<dyn CostStore>, user_id: &str, cost: MicroUsd) {
        use aura_model::{JobId, SessionId, SpanId};
        store
            .record(&CostRecord {
                user_id: user_id.into(),
                session_id: SessionId::from("s1"),
                job_id: JobId::new(),
                span_id: SpanId::new(),
                model: "m1".into(),
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_usd: cost,
                timestamp: Utc::now(),
            })
            .await
            .unwrap();
    }

    fn usd(v: f64) -> MicroUsd {
        MicroUsd::from_usd_decimal(v)
    }

    // ── CostManager ───────────────────────────────────────────────

    fn manager_with(limits: SpendingLimits) -> Arc<CostManager> {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        CostManager::new(store, pricing("m1", 3.0, 15.0), limits)
    }

    /// Drive `record_call` through the public API, then wait for the
    /// fire-and-forget persist to finish before returning.
    async fn record_then_drain(
        cm: &Arc<CostManager>,
        user_id: &str,
        input_tokens: usize,
        output_tokens: usize,
    ) {
        cm.record_call(
            user_id,
            SessionId::from("s1"),
            JobId::new(),
            SpanId::new(),
            "m1",
            input_tokens,
            output_tokens,
            0,
            0,
        );
        // record_call returns sync after updating in-memory state and
        // spawning the persist task. Yield until the metrics counter
        // ticks so persist-dependent assertions are deterministic.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        let target = cm.metrics().recorded() + 1;
        while cm.metrics().recorded() < target {
            if std::time::Instant::now() >= deadline {
                panic!("persist task never completed");
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn set_limits_is_seen_by_next_check() {
        // No caps configured → spend is unconstrained.
        let cm = manager_with(SpendingLimits::default());
        record_then_drain(&cm, "u", 1_000_000, 0).await;
        assert!(cm.check().is_ok(), "uncapped check should pass");

        // Hot-swap in a daily cap below the accrued spend; the very next
        // check must observe it (the config hot-reload path).
        cm.set_limits(SpendingLimits {
            daily_usd: Some(usd(0.000_001)),
            monthly_usd: None,
        });
        assert!(
            matches!(cm.check(), Err(CostGuardError::DailyLimitExceeded { .. })),
            "check must observe the swapped-in cap",
        );
    }

    #[tokio::test]
    async fn check_rejects_at_or_over_daily_cap() {
        let cm = manager_with(SpendingLimits {
            daily_usd: Some(usd(0.003)),
            ..Default::default()
        });
        // 1k input @ $3/MTok = $0.003 — boundary case, `>=` trips.
        cm.record_call(
            "u1",
            SessionId::from("s1"),
            JobId::new(),
            SpanId::new(),
            "m1",
            1_000,
            0,
            0,
            0,
        );
        assert!(matches!(
            cm.check(),
            Err(CostGuardError::DailyLimitExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn check_rejects_at_or_over_monthly_cap() {
        let cm = manager_with(SpendingLimits {
            monthly_usd: Some(usd(0.003)),
            ..Default::default()
        });
        cm.record_call(
            "u1",
            SessionId::from("s1"),
            JobId::new(),
            SpanId::new(),
            "m1",
            1_000,
            0,
            0,
            0,
        );
        assert!(matches!(
            cm.check(),
            Err(CostGuardError::MonthlyLimitExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn check_passes_when_no_caps_configured() {
        let cm = manager_with(SpendingLimits::default());
        cm.record_call(
            "u1",
            SessionId::from("s1"),
            JobId::new(),
            SpanId::new(),
            "m1",
            1_000_000_000,
            0,
            0,
            0,
        );
        assert!(cm.check().is_ok());
    }

    #[tokio::test]
    async fn record_call_updates_state_synchronously() {
        // Hot-path invariant: by the time record_call returns, check
        // already sees the new spend.
        let cm = manager_with(SpendingLimits {
            daily_usd: Some(usd(0.003)),
            ..Default::default()
        });
        cm.record_call(
            "u1",
            SessionId::from("s1"),
            JobId::new(),
            SpanId::new(),
            "m1",
            1_000,
            0,
            0,
            0,
        );
        // Synchronous: no .await between record_call return and check.
        assert!(cm.check().is_err());
    }

    #[tokio::test]
    async fn record_call_persists_asynchronously() {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        let cm = CostManager::new(
            Arc::clone(&store),
            pricing("m1", 3.0, 15.0),
            SpendingLimits::default(),
        );
        record_then_drain(&cm, "u1", 1_000, 2_000).await;
        let records = store
            .query_user(
                "u1",
                TimeRange {
                    from: Utc::now() - chrono::Duration::hours(1),
                    to: Utc::now() + chrono::Duration::hours(1),
                },
            )
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, 1_000);
        assert_eq!(records[0].output_tokens, 2_000);
        assert_eq!(cm.metrics().recorded(), 1);
    }

    #[tokio::test]
    async fn hydrate_seeds_state_from_persisted_records() {
        let store: Arc<dyn CostStore> = Arc::new(MemoryCostStore::default());
        record_spend(&store, "u1", usd(0.6)).await;
        record_spend(&store, "u2", usd(0.5)).await;
        let cm = CostManager::new(
            Arc::clone(&store),
            pricing("m1", 3.0, 15.0),
            SpendingLimits {
                daily_usd: Some(usd(1.0)),
                ..Default::default()
            },
        );
        cm.hydrate().await;
        // Hydrated total $1.10 ≥ $1.00 → check rejects before any
        // new event lands. Protects against post-restart wide-open
        // budgets and proves spend across users sums into one pool.
        assert!(matches!(
            cm.check(),
            Err(CostGuardError::DailyLimitExceeded { .. })
        ));
    }

    #[test]
    fn check_treats_stale_window_as_zero_spend() {
        // Build state directly so we can backdate `daily_date` —
        // record_call always uses Utc::now and can't drive rollover.
        let cm = manager_with(SpendingLimits {
            daily_usd: Some(usd(0.001)),
            ..Default::default()
        });
        let yesterday = Utc::now() - chrono::Duration::days(1);
        {
            let mut state = cm.state.write();
            state.daily_date = Some(yesterday.date_naive());
            state.daily = MicroUsd::from_usd_decimal(0.999);
        }
        // check() at "today" must read yesterday's window as stale
        // (zero spend) and pass.
        assert!(cm.check().is_ok(), "stale daily window must read as zero");
    }
}
