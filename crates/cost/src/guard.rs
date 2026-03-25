use std::sync::Arc;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

use crate::{TimeRange, tracker::CostTracker};

/// Spending limits that `CostGuard` enforces.
///
/// Each field is optional — `None` means "no limit" for that dimension.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostLimits {
    pub daily_per_user: Option<f64>,
    pub monthly_per_user: Option<f64>,
    pub daily_global: Option<f64>,
    pub monthly_global: Option<f64>,
}

/// The outcome of a limit check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CostDecision {
    Allow,
    Reject {
        reason: String,
        current: f64,
        limit: f64,
    },
}

/// Pre-execution spending guard.
///
/// `CostGuard` only decides whether a request should proceed — it never records costs.
/// It checks per-user daily/monthly and global daily/monthly limits in order,
/// returning the first rejection encountered.
pub struct CostGuard {
    cost_tracker: Arc<CostTracker>,
    limits: CostLimits,
}

impl CostGuard {
    pub fn new(cost_tracker: Arc<CostTracker>, limits: CostLimits) -> Self {
        Self {
            cost_tracker,
            limits,
        }
    }

    /// Check all per-user limits (daily, then monthly).
    pub async fn check_user_limit(
        &self,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> aura_core::Result<CostDecision> {
        if let Some(daily_limit) = self.limits.daily_per_user {
            let range = daily_range(now);
            let current = self.cost_tracker.sum_user(user_id, range).await?;
            if current >= daily_limit {
                return Ok(CostDecision::Reject {
                    reason: format!("user daily limit exceeded for user {user_id}"),
                    current,
                    limit: daily_limit,
                });
            }
        }

        if let Some(monthly_limit) = self.limits.monthly_per_user {
            let range = monthly_range(now);
            let current = self.cost_tracker.sum_user(user_id, range).await?;
            if current >= monthly_limit {
                return Ok(CostDecision::Reject {
                    reason: format!("user monthly limit exceeded for user {user_id}"),
                    current,
                    limit: monthly_limit,
                });
            }
        }

        Ok(CostDecision::Allow)
    }

    /// Check all global limits (daily, then monthly).
    pub async fn check_global_limit(&self, now: DateTime<Utc>) -> aura_core::Result<CostDecision> {
        if let Some(daily_limit) = self.limits.daily_global {
            let range = daily_range(now);
            let summary = self.cost_tracker.query_global(range).await?;
            if summary.total_cost_usd >= daily_limit {
                return Ok(CostDecision::Reject {
                    reason: "global daily limit exceeded".to_string(),
                    current: summary.total_cost_usd,
                    limit: daily_limit,
                });
            }
        }

        if let Some(monthly_limit) = self.limits.monthly_global {
            let range = monthly_range(now);
            let summary = self.cost_tracker.query_global(range).await?;
            if summary.total_cost_usd >= monthly_limit {
                return Ok(CostDecision::Reject {
                    reason: "global monthly limit exceeded".to_string(),
                    current: summary.total_cost_usd,
                    limit: monthly_limit,
                });
            }
        }

        Ok(CostDecision::Allow)
    }
}

/// Build a `TimeRange` covering the UTC day containing `now`.
fn daily_range(now: DateTime<Utc>) -> TimeRange {
    let from = now.date_naive().and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
    let to = now
        .date_naive()
        .succ_opt()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc());

    // Both `from` and `to` are always valid for any real UTC date, so this is safe.
    // We still avoid unwrap by falling back to `now` in the astronomically unlikely case.
    TimeRange {
        from: from.unwrap_or(now),
        to: to.unwrap_or(now),
    }
}

/// Build a `TimeRange` covering the UTC month containing `now`.
fn monthly_range(now: DateTime<Utc>) -> TimeRange {
    let first_of_month = now
        .date_naive()
        .with_day(1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc());

    // Advance to next month: if month == 12, roll over to January of next year.
    let (next_year, next_month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };

    let first_of_next_month = chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc());

    TimeRange {
        from: first_of_month.unwrap_or(now),
        to: first_of_next_month.unwrap_or(now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_range_covers_full_day() {
        let now = DateTime::parse_from_rfc3339("2026-03-22T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let range = daily_range(now);
        assert_eq!(
            range.from,
            DateTime::parse_from_rfc3339("2026-03-22T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(
            range.to,
            DateTime::parse_from_rfc3339("2026-03-23T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn monthly_range_covers_full_month() {
        let now = DateTime::parse_from_rfc3339("2026-03-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let range = monthly_range(now);
        assert_eq!(
            range.from,
            DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(
            range.to,
            DateTime::parse_from_rfc3339("2026-04-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn monthly_range_december_rolls_over() {
        let now = DateTime::parse_from_rfc3339("2026-12-25T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let range = monthly_range(now);
        assert_eq!(
            range.from,
            DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(
            range.to,
            DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }
}
