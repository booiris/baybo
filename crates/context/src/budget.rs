/// Token budget tracking for the context window.
///
/// Tracks current token usage and determines when compression is needed
/// based on a configurable threshold relative to the maximum token limit.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    max_tokens: usize,
    threshold: f64,
    current: usize,
}

impl TokenBudget {
    pub fn new(max_tokens: usize, threshold: f64) -> Self {
        Self {
            max_tokens,
            threshold,
            current: 0,
        }
    }

    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn remaining(&self) -> usize {
        self.max_tokens.saturating_sub(self.current)
    }

    /// Whether the current token count exceeds the compression threshold.
    pub fn needs_compression(&self) -> bool {
        self.current > (self.max_tokens as f64 * self.threshold) as usize
    }

    /// Update the tracked token count.
    pub fn update(&mut self, token_count: usize) {
        self.current = token_count;
    }

    /// Replace the maximum token cap. Used when the actor switches to
    /// a model with a different context window — the configured budget
    /// from `aura.json` is the upper bound, but if the active model's
    /// window is smaller we clamp down so compression triggers before
    /// the provider rejects the request. `current` is unchanged so the
    /// next `needs_compression()` check sees the new cap immediately.
    pub fn set_max_tokens(&mut self, max_tokens: usize) {
        self.max_tokens = max_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_compression_respects_threshold() {
        let mut budget = TokenBudget::new(100, 0.75);
        budget.update(74);
        assert!(!budget.needs_compression());
        budget.update(76);
        assert!(budget.needs_compression());
    }

    #[test]
    fn remaining_saturates_at_zero() {
        let mut budget = TokenBudget::new(100, 0.75);
        budget.update(150);
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn set_max_tokens_lowers_cap_and_flips_needs_compression() {
        let mut budget = TokenBudget::new(1_000, 0.75);
        budget.update(500);
        // At max=1000, threshold 0.75 → trigger at 751; current 500 fits.
        assert!(!budget.needs_compression());
        // Shrinking to a 600-token context window pushes us over the
        // new threshold (600 * 0.75 = 450) without changing `current`.
        budget.set_max_tokens(600);
        assert_eq!(budget.max_tokens(), 600);
        assert!(budget.needs_compression());
    }
}
