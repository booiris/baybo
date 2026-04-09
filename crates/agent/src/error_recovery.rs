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
    ///
    /// Since the agent uses `anyhow::Error`, we inspect the error message
    /// to classify transient vs non-retryable errors. Errors containing
    /// "timeout" or "io error" are considered transient and retryable.
    /// Security-related errors are not retried.
    pub fn should_retry(&self, attempt: u32, error: &anyhow::Error) -> bool {
        if attempt >= self.max_retries {
            return false;
        }
        let msg = error.to_string().to_lowercase();
        if msg.contains("security") {
            warn!(attempt, error = %error, "non-retryable security error");
            return false;
        }
        // Retry transient errors (timeout, io, internal)
        if msg.contains("timeout") || msg.contains("io error") {
            return true;
        }
        // Also retry if the error chain contains a known transient module error
        if error.downcast_ref::<aura_llm::LlmError>().is_some() {
            return true;
        }
        warn!(attempt, error = %error, "non-retryable error");
        false
    }
}
