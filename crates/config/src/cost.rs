use serde::{Deserialize, Serialize};

/// Cost and rate limit configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CostConfig {
    pub spending_limits: SpendingLimitsConfig,
    pub rate_limit: RateLimitConfig,
}

/// Spending caps in USD. Any field left as `None` is treated as unlimited.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SpendingLimitsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_daily_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_monthly_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_daily_usd: Option<f64>,
}

/// Per-user request rate limit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Maximum requests allowed within the window.
    pub max_requests: usize,
    /// Sliding window duration in seconds.
    pub window_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 30,
            window_secs: 60,
        }
    }
}
