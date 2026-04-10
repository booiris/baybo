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
}
