use std::sync::Arc;

use aura_storage::{CostRecord, CostStore, TimeRange};
use chrono::{Datelike, NaiveDate, Utc};
use thiserror::Error;

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
