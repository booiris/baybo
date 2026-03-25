use std::time::Duration;

use tracing::warn;

/// Error recovery handler with exponential backoff.
#[derive(Debug, Clone)]
pub struct ErrorHandler {
    pub max_retries: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for ErrorHandler {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
        }
    }
}

impl ErrorHandler {
    /// Calculate the backoff duration for a given retry attempt.
    pub fn backoff_duration(&self, attempt: u32) -> Duration {
        let backoff = self
            .backoff_base
            .saturating_mul(2u32.saturating_pow(attempt));
        if backoff > self.backoff_max {
            self.backoff_max
        } else {
            backoff
        }
    }

    /// Determine whether a retry should be attempted.
    pub fn should_retry(&self, attempt: u32, error: &aura_core::AuraError) -> bool {
        if attempt >= self.max_retries {
            return false;
        }
        // Only retry transient errors, not configuration or security errors.
        match error {
            aura_core::AuraError::Internal(_) | aura_core::AuraError::Timeout(_) => true,
            aura_core::AuraError::Io(_) => true,
            _ => {
                warn!(attempt, error = %error, "non-retryable error");
                false
            }
        }
    }
}
