use aura_model::{MicroUsd, usd_decimal_option};
use serde::{Deserialize, Serialize};

/// Cost and rate limit configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CostConfig {
    pub spending_limits: SpendingLimitsConfig,
    pub rate_limit: RateLimitConfig,
}

/// Total spending caps. Personal-assistant deployment — there's no
/// per-user dimension. TOML wire format keeps the human-friendly USD
/// decimal (operators write `daily_usd = 5.0`); the
/// `usd_decimal_option` bridge converts to integer micro-USD on load.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SpendingLimitsConfig {
    #[serde(skip_serializing_if = "Option::is_none", with = "usd_decimal_option")]
    pub daily_usd: Option<MicroUsd>,
    #[serde(skip_serializing_if = "Option::is_none", with = "usd_decimal_option")]
    pub monthly_usd: Option<MicroUsd>,
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
