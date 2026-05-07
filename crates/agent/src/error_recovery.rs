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

    /// Type-driven retry decision:
    /// - `LlmError`: defers to [`aura_llm::LlmError::is_retriable`].
    /// - `std::io::Error`: always retried — transport flake.
    /// - Anything else: do not retry. An unrecognised error shape is
    ///   safer to surface than to silently re-issue.
    pub fn should_retry(&self, attempt: u32, error: &anyhow::Error) -> bool {
        if attempt >= self.max_retries {
            return false;
        }
        if let Some(llm_err) = error.downcast_ref::<aura_llm::LlmError>() {
            return llm_err.is_retriable();
        }
        if error.downcast_ref::<std::io::Error>().is_some() {
            return true;
        }
        warn!(attempt, error = %error, "non-retryable error (unknown type)");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_llm::LlmError;

    fn handler() -> ErrorHandler {
        ErrorHandler::default()
    }

    #[test]
    fn guard_rejected_llm_error_is_not_retried() {
        let h = handler();
        let err = anyhow::Error::new(LlmError::GuardRejected("daily limit exceeded".into()));
        assert!(
            !h.should_retry(0, &err),
            "budget-rejected calls must not consume retry attempts"
        );
    }

    #[test]
    fn transient_llm_error_is_retried_until_cap() {
        let h = handler();
        let err = anyhow::Error::new(LlmError::Transient("upstream 503".into()));
        assert!(h.should_retry(0, &err));
        assert!(h.should_retry(h.max_retries - 1, &err));
        assert!(!h.should_retry(h.max_retries, &err));
    }

    #[test]
    fn config_and_model_errors_are_not_retried() {
        let h = handler();
        let cfg = anyhow::Error::new(LlmError::Config("bad api key".into()));
        let nm = anyhow::Error::new(LlmError::ModelNotFound("nope-3".into()));
        assert!(!h.should_retry(0, &cfg));
        assert!(!h.should_retry(0, &nm));
    }
}
