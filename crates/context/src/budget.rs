/// Token budget tracking for the context window.
///
/// Tracks current token usage and determines when compression is needed.
/// Two rules decide that, and the tighter one wins: a share of the active
/// model's window, and an absolute ceiling on how much context is worth
/// carrying at all.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    max_tokens: usize,
    threshold: f64,
    /// Absolute cap on the active context, whatever the window allows.
    /// `0` disables it, leaving the window share as the only rule.
    max_active: usize,
    current: usize,
}

impl TokenBudget {
    pub fn new(max_tokens: usize, threshold: f64, max_active: usize) -> Self {
        Self {
            max_tokens,
            threshold,
            max_active,
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
        self.current > self.compression_ceiling()
    }

    /// The token count a compaction has to land at or below to have been
    /// worth running: crossing it is what triggers the next one.
    ///
    /// The window share alone stopped being a bound when providers started
    /// advertising million-token windows: at 0.65 of 1,048,576 a
    /// conversation compacts at 681K, which no run reaches before the cost
    /// and the latency of carrying that prefix have already been paid on
    /// every call. The absolute cap is what makes the ceiling mean "more
    /// context than is worth carrying" rather than "nearly too much to
    /// send".
    pub fn compression_ceiling(&self) -> usize {
        let share = (self.max_tokens as f64 * self.threshold) as usize;
        if self.max_active == 0 {
            return share;
        }
        share.min(self.max_active)
    }

    /// Update the tracked token count.
    pub fn update(&mut self, token_count: usize) {
        self.current = token_count;
    }

    /// Install the active model's context window as the cap. Called
    /// when an actor swaps LLMs so compression triggers before the
    /// new provider rejects an oversized request. `current` is
    /// unchanged so the next `needs_compression()` check sees the
    /// new cap immediately.
    pub fn set_max_tokens(&mut self, max_tokens: usize) {
        self.max_tokens = max_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_compression_respects_threshold() {
        let mut budget = TokenBudget::new(100, 0.75, 0);
        budget.update(74);
        assert!(!budget.needs_compression());
        budget.update(76);
        assert!(budget.needs_compression());
    }

    #[test]
    fn remaining_saturates_at_zero() {
        let mut budget = TokenBudget::new(100, 0.75, 0);
        budget.update(150);
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn set_max_tokens_lowers_cap_and_flips_needs_compression() {
        let mut budget = TokenBudget::new(1_000, 0.75, 0);
        budget.update(500);
        // At max=1000, threshold 0.75 → trigger at 751; current 500 fits.
        assert!(!budget.needs_compression());
        // Shrinking to a 600-token context window pushes us over the
        // new threshold (600 * 0.75 = 450) without changing `current`.
        budget.set_max_tokens(600);
        assert_eq!(budget.max_tokens(), 600);
        assert!(budget.needs_compression());
    }

    /// The case this cap exists for, at the real numbers: a million-token
    /// window puts the window share at 681K, which the longest observed
    /// run (330K) never reached — so nothing ever compacted and every call
    /// carried the whole transcript.
    #[test]
    fn a_million_token_window_still_compacts_at_the_absolute_cap() {
        let mut budget = TokenBudget::new(1_048_576, 0.65, 120_000);
        assert_eq!(budget.compression_ceiling(), 120_000);
        budget.update(330_527);
        assert!(
            budget.needs_compression(),
            "the run that peaked at 330K must compact"
        );
    }

    #[test]
    fn the_tighter_of_the_two_rules_wins() {
        // A small window is still governed by its own share: the cap must
        // never *raise* a ceiling.
        let budget = TokenBudget::new(32_000, 0.65, 120_000);
        assert_eq!(budget.compression_ceiling(), 20_800);

        // And the cap is off when it is zero, whatever the window.
        let uncapped = TokenBudget::new(1_048_576, 0.65, 0);
        assert_eq!(uncapped.compression_ceiling(), 681_574);
    }
}
