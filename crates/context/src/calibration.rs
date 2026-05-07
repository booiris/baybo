//! Per-model calibration of `Tokenizer` estimates against real LLM
//! response usage.
//!
//! `TiktokenTokenizer` uses OpenAI's BPE families and is at best a
//! ~10% approximation for non-OpenAI providers (Anthropic, Gemini,
//! …). It also under-counts because per-message estimates don't
//! include the request envelope (system prompt, tools schema, etc.)
//! that the provider folds into `usage.input_tokens`.
//!
//! [`TokenCalibration`] tracks an exponentially-weighted ratio
//! `actual / estimate` per `model_id`. `ContextManager::count_tokens`
//! multiplies its raw estimate by this ratio so budget triggers and
//! compression decisions match real input usage. The agent loop feeds
//! samples back via `observe` after each main LLM call (compression
//! calls are skipped — their input shape differs and would noise the
//! ratio).
//!
//! State is in-memory and per-process. After a few main-call samples
//! the ratio stabilises; cold start or an unobserved model uses 1.0
//! (raw estimate).

use std::collections::HashMap;

use parking_lot::RwLock;

/// EMA factor — weight given to each new sample. After ~5 samples a
/// step-change in the underlying rate is mostly absorbed.
const EMA_ALPHA: f64 = 0.3;

/// Skip samples below this raw-estimate threshold. Tiny calls have
/// large *relative* envelope overhead and would push the ratio toward
/// pathological values.
const MIN_ESTIMATE_FOR_SAMPLE: usize = 100;

/// Per-sample clamp. Caps a single observation's influence so a
/// malformed response or a one-off provider hiccup can't shove the
/// ratio outside reasonable bounds.
const SAMPLE_RATIO_MIN: f64 = 0.5;
const SAMPLE_RATIO_MAX: f64 = 2.0;

#[derive(Debug, Clone, Copy)]
struct ModelCalibration {
    ratio: f64,
    samples: u64,
}

impl Default for ModelCalibration {
    fn default() -> Self {
        Self {
            ratio: 1.0,
            samples: 0,
        }
    }
}

/// Thread-safe per-model calibration store. Cheap to share via
/// `Arc<TokenCalibration>` across `ContextManager` and `AgentLoop`.
#[derive(Default)]
pub struct TokenCalibration {
    state: RwLock<HashMap<String, ModelCalibration>>,
}

impl TokenCalibration {
    pub fn new() -> Self {
        Self::default()
    }

    /// Multiply `raw_estimate` by the current ratio for `model_id`.
    /// Unobserved models pass through unchanged (ratio = 1.0).
    pub fn adjust(&self, model_id: &str, raw_estimate: usize) -> usize {
        let ratio = self
            .state
            .read()
            .get(model_id)
            .map(|m| m.ratio)
            .unwrap_or(1.0);
        ((raw_estimate as f64) * ratio).round().max(0.0) as usize
    }

    /// Feed back a `(raw_estimate, actual)` pair from a real LLM call.
    /// Drops samples that are too small or yield a degenerate ratio.
    pub fn observe(&self, model_id: &str, raw_estimate: usize, actual: usize) {
        if raw_estimate < MIN_ESTIMATE_FOR_SAMPLE || actual == 0 {
            return;
        }
        let sample =
            ((actual as f64) / (raw_estimate as f64)).clamp(SAMPLE_RATIO_MIN, SAMPLE_RATIO_MAX);
        // Fast path: per-model entry already exists. Steady state hits
        // this branch every call after the first per-model observe, so
        // we avoid the `to_string()` allocation that `entry` requires.
        if let Some(entry) = self.state.write().get_mut(model_id) {
            entry.ratio = EMA_ALPHA * sample + (1.0 - EMA_ALPHA) * entry.ratio;
            entry.samples = entry.samples.saturating_add(1);
            return;
        }
        let mut state = self.state.write();
        let entry = state.entry(model_id.to_string()).or_default();
        entry.ratio = EMA_ALPHA * sample + (1.0 - EMA_ALPHA) * entry.ratio;
        entry.samples = entry.samples.saturating_add(1);
    }

    /// Returns `None` for unobserved models so a caller can distinguish
    /// "no data yet" from "ratio happens to be 1.0 after observations".
    pub fn ratio(&self, model_id: &str) -> Option<f64> {
        self.state.read().get(model_id).map(|m| m.ratio)
    }

    pub fn sample_count(&self, model_id: &str) -> u64 {
        self.state
            .read()
            .get(model_id)
            .map(|m| m.samples)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "test-model";

    #[test]
    fn unobserved_model_passes_estimate_through() {
        let cal = TokenCalibration::new();
        assert_eq!(cal.adjust(MODEL, 1_000), 1_000);
        assert!(cal.ratio(MODEL).is_none());
        assert_eq!(cal.sample_count(MODEL), 0);
    }

    #[test]
    fn first_observation_pulls_ratio_toward_sample() {
        let cal = TokenCalibration::new();
        // Estimate 1000, actual 1500 → sample = 1.5
        cal.observe(MODEL, 1_000, 1_500);
        let r = cal.ratio(MODEL).expect("ratio set after observe");
        // EMA: 0.3 * 1.5 + 0.7 * 1.0 = 1.15
        assert!((r - 1.15).abs() < 1e-9, "got {r}");
        assert_eq!(cal.sample_count(MODEL), 1);
    }

    #[test]
    fn repeated_observations_converge() {
        let cal = TokenCalibration::new();
        // Steady-state actual = 1.4 * estimate. After many samples,
        // the EMA should sit very close to 1.4.
        for _ in 0..50 {
            cal.observe(MODEL, 1_000, 1_400);
        }
        let r = cal.ratio(MODEL).unwrap();
        assert!((r - 1.4).abs() < 0.01, "ratio did not converge: {r}");
    }

    #[test]
    fn adjust_applies_calibrated_ratio() {
        let cal = TokenCalibration::new();
        for _ in 0..20 {
            cal.observe(MODEL, 1_000, 1_500);
        }
        let r = cal.ratio(MODEL).unwrap();
        assert!(r > 1.4 && r < 1.51, "ratio out of expected band: {r}");
        let adjusted = cal.adjust(MODEL, 2_000);
        let expected = ((2_000.0 * r).round()) as usize;
        assert_eq!(adjusted, expected);
    }

    #[test]
    fn small_estimates_are_skipped() {
        let cal = TokenCalibration::new();
        cal.observe(MODEL, 50, 1_000); // below MIN_ESTIMATE_FOR_SAMPLE
        assert!(cal.ratio(MODEL).is_none());
        assert_eq!(cal.sample_count(MODEL), 0);
    }

    #[test]
    fn zero_actual_is_skipped() {
        let cal = TokenCalibration::new();
        cal.observe(MODEL, 1_000, 0);
        assert!(cal.ratio(MODEL).is_none());
    }

    #[test]
    fn samples_are_clamped_against_outliers() {
        let cal = TokenCalibration::new();
        // Pathological: actual is 100x the estimate. Without clamping
        // a single sample would crater the ratio. With clamp at 2.0
        // and α=0.3, post-sample ratio = 0.3 * 2.0 + 0.7 * 1.0 = 1.3.
        cal.observe(MODEL, 1_000, 100_000);
        let r = cal.ratio(MODEL).unwrap();
        assert!((r - 1.3).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn each_model_has_independent_state() {
        let cal = TokenCalibration::new();
        cal.observe("model-a", 1_000, 1_500); // pushes ratio up
        cal.observe("model-b", 1_000, 700); // pushes ratio down

        let a = cal.ratio("model-a").unwrap();
        let b = cal.ratio("model-b").unwrap();
        assert!(a > 1.0, "model-a should have ratio > 1.0, got {a}");
        assert!(b < 1.0, "model-b should have ratio < 1.0, got {b}");
        assert!((a - 1.15).abs() < 1e-9);
        assert!((b - 0.91).abs() < 1e-9, "got {b}");
    }
}
