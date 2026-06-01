//! Shared data layer for the memory benchmark: scope keys, the ingest↔run
//! manifest, and the deterministic F1 metric. Holds **no Aura dependencies** so
//! it stays fast to compile and easy to unit-test.
//!
//! Dataset parsing lives behind the [`testset`] seam — each [`testset::TestSet`]
//! normalizes its own on-disk format into the shared `BenchSample` IR, so the
//! ingest/run harness below is dataset-agnostic.

use serde::{Deserialize, Serialize};

pub mod agent;
pub mod backend;
pub mod judge;
pub mod llm;
pub mod report;
pub mod testset;

/// Env var holding the mem0 API key (mirrors `aura_memory::backends::mem0`).
pub const MEM0_API_KEY_ENV: &str = "MEM0_API_KEY";

// ---------------------------------------------------------------------------
// Scope keys (isolation)
// ---------------------------------------------------------------------------

/// The per-conversation isolation key. Unique across test sets (via `testset`),
/// runs (via `run_id`), and arms, so reruns never silently contaminate each
/// other — and two benchmarks sharing one backend can't collide.
pub fn scope_user_id(testset: &str, run_id: &str, arm: &str, conv_idx: usize) -> String {
    format!("{testset}-{run_id}-{arm}-conv{conv_idx}")
}

/// The per-session id used during ingest (openviking commits per session). QA
/// uses a fresh session id under the same `user_id`.
pub fn scope_session_id(user_id: &str, session_index: usize) -> String {
    format!("{user_id}-s{session_index}")
}

// ---------------------------------------------------------------------------
// Scoring — token-overlap F1 (secondary, deterministic sanity metric)
// ---------------------------------------------------------------------------

fn normalize_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// SQuAD-style token-overlap F1 between a gold and candidate answer. Two empty
/// strings score 1.0; one empty scores 0.0.
pub fn token_f1(gold: &str, candidate: &str) -> f64 {
    use std::collections::HashMap;
    let gold = normalize_tokens(gold);
    let cand = normalize_tokens(candidate);
    if gold.is_empty() || cand.is_empty() {
        return if gold.is_empty() && cand.is_empty() {
            1.0
        } else {
            0.0
        };
    }
    let mut gold_counts: HashMap<&str, usize> = HashMap::new();
    for tok in &gold {
        *gold_counts.entry(tok.as_str()).or_default() += 1;
    }
    let mut overlap = 0usize;
    let mut cand_counts: HashMap<&str, usize> = HashMap::new();
    for tok in &cand {
        *cand_counts.entry(tok.as_str()).or_default() += 1;
    }
    for (tok, cc) in &cand_counts {
        if let Some(gc) = gold_counts.get(tok) {
            overlap += (*gc).min(*cc);
        }
    }
    if overlap == 0 {
        return 0.0;
    }
    let precision = overlap as f64 / cand.len() as f64;
    let recall = overlap as f64 / gold.len() as f64;
    2.0 * precision * recall / (precision + recall)
}

// ---------------------------------------------------------------------------
// Manifest — couples the ingest and run phases
// ---------------------------------------------------------------------------

/// Produced by `ingest`, consumed by `run`: maps each conversation to the scope
/// the backend was populated under, so QA's `recall` queries the right keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub run_id: String,
    pub testset: String,
    pub dataset: String,
    pub arm: String,
    pub settled: bool,
    pub conversations: Vec<ConvScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvScope {
    pub conv_idx: usize,
    pub sample_id: Option<String>,
    pub user_id: String,
    pub session_ids: Vec<String>,
    pub turn_pairs: usize,
    pub memories_stored: usize,
}

impl Manifest {
    pub fn user_id_for(&self, conv_idx: usize) -> Option<&str> {
        self.conversations
            .iter()
            .find(|c| c.conv_idx == conv_idx)
            .map(|c| c.user_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f1_scores() {
        assert_eq!(token_f1("biscuit", "biscuit"), 1.0);
        assert_eq!(token_f1("", ""), 1.0);
        assert_eq!(token_f1("biscuit", ""), 0.0);
        assert_eq!(token_f1("a puppy named biscuit", "the cat"), 0.0);
        let partial = token_f1("a puppy named Biscuit", "Biscuit, a puppy");
        assert!(partial > 0.5 && partial < 1.0, "got {partial}");
    }

    #[test]
    fn scope_keys_are_unique_and_stable() {
        let u = scope_user_id("locomo", "run42", "mem0", 3);
        assert_eq!(u, "locomo-run42-mem0-conv3");
        assert_eq!(scope_session_id(&u, 2), "locomo-run42-mem0-conv3-s2");
        assert_ne!(
            scope_user_id("locomo", "run42", "mem0", 3),
            scope_user_id("locomo", "run43", "mem0", 3)
        );
        assert_ne!(
            scope_user_id("locomo", "run42", "mem0", 3),
            scope_user_id("locomo", "run42", "openviking", 3)
        );
        // Different test sets never collide in a shared backend.
        assert_ne!(
            scope_user_id("locomo", "run42", "mem0", 3),
            scope_user_id("longmemeval", "run42", "mem0", 3)
        );
    }
}
