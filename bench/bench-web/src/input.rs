//! Lightweight `Deserialize` mirrors of each bench's on-disk result
//! JSON. Deliberately separate read-models from the producer types in
//! `aura-bench-swe` / `aura-bench-memory`: depending on those crates
//! would drag the whole agent runtime into this viewer's build, and the
//! read side wants `#[serde(default)]` tolerance for fields that vary
//! across schema versions and across arms.
//!
//! Field names mirror the producers (`bench/swe/src/report.rs`,
//! `bench/memory/src/report.rs`) and the terminal-bench / Harbor harness
//! output. Anything absent defaults rather than failing the parse.

use std::collections::BTreeMap;

use serde::Deserialize;

// ── SWE-bench (results-<arm>-<run>.json / merged-<arm>.json) ──────────

#[derive(Debug, Clone, Deserialize)]
pub struct SweRun {
    pub run_id: String,
    #[serde(default)]
    pub dataset: Option<String>,
    #[serde(default)]
    pub arm: String,
    #[serde(default)]
    pub model: Option<String>,
    // Float-tolerant: the cross-run `merged-*` consolidation writes this
    // as a fractional mean (e.g. 182740.5), while individual runs write
    // an integer.
    #[serde(default)]
    pub mean_latency_ms: f64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub total_cost_micro_usd: i64,
    #[serde(default)]
    pub results: Vec<SweItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SweItem {
    pub instance_id: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub empty_patch: bool,
    #[serde(default)]
    pub errored: bool,
    #[serde(default)]
    pub patch_bytes: u64,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cost_micro_usd: i64,
    #[serde(default)]
    pub error: Option<String>,
    /// Grading-side reason an unresolved instance failed (which FAIL_TO_PASS
    /// tests, a PASS_TO_PASS regression, or an apply failure). Absent on older runs.
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub source_run: Option<String>,
}

// ── Memory / LOCOMO (results-<arm>-<run>.json / merged-<arm>.json) ────

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryRun {
    pub run_id: String,
    #[serde(default)]
    pub testset: Option<String>,
    #[serde(default)]
    pub arm: String,
    #[serde(default)]
    pub answer_model: Option<String>,
    #[serde(default)]
    pub overall_accuracy: f64,
    #[serde(default)]
    pub mean_f1: f64,
    // Float-tolerant, like SweRun::mean_latency_ms (merged consolidation
    // writes a fractional mean).
    #[serde(default)]
    pub mean_latency_ms: f64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub total_cost_micro_usd: i64,
    #[serde(default)]
    pub results: Vec<MemoryItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryItem {
    #[serde(default)]
    pub conv_idx: usize,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub gold: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub correct: bool,
    #[serde(default)]
    pub f1: f64,
    #[serde(default)]
    pub judge_reason: String,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cost_micro_usd: i64,
    #[serde(default)]
    pub source_run: Option<String>,
}

// ── terminal-bench 1.0 + 2.0 (results-<ts>.json / merged.json) ────────
//
// One struct covers both: 2.0 (Harbor) adds `reward` + `trace_path` and
// drops the per-phase timestamps; 1.0 adds the timestamps + token
// totals + `recording_path` and omits `trace_path` on individual runs.
// `merged.json` (both versions) adds `source_run` + `trace_path`.

#[derive(Debug, Clone, Deserialize)]
pub struct TbRun {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub results: Vec<TbItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TbItem {
    pub task_id: String,
    #[serde(default)]
    pub trial_name: String,
    #[serde(default)]
    pub instruction: Option<String>,
    #[serde(default)]
    pub is_resolved: bool,
    #[serde(default)]
    pub failure_mode: String,
    #[serde(default)]
    pub parser_results: BTreeMap<String, String>,
    #[serde(default)]
    pub recording_path: Option<String>,
    /// Present on 2.0 individual runs and on both versions' merged.json;
    /// relative to the bench's `trace/` dir.
    #[serde(default)]
    pub trace_path: Option<String>,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cached_input_tokens: u64,
    #[serde(default)]
    pub trial_started_at: Option<String>,
    #[serde(default)]
    pub trial_ended_at: Option<String>,
    #[serde(default)]
    pub source_run: Option<String>,
}

/// terminal-bench 1.0 `runs/<ts>/run_metadata.json` — the model / dataset
/// / wall-clock the results file itself doesn't carry.
#[derive(Debug, Clone, Deserialize)]
pub struct TbRunMetadata {
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub dataset_name: Option<String>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
}
