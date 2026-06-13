//! The normalized "spine" the four bench adapters feed and the API
//! serves. Shared fields are strongly typed; each bench's idiosyncratic
//! detail rides in [`BenchExtra`]. Built from the on-disk `results/`
//! artifacts by [`crate::adapters`].
//!
//! 64-bit numeric fields carry an explicit `ts(type = "number")` so the
//! generated TypeScript treats them as `number` (JSON-safe, well within
//! 2^53 for tokens/cost-micro-USD/latency) rather than ts-rs's default
//! `bigint`.

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts-export")]
use ts_rs::TS;

/// Which bench family an [`Item`]'s [`BenchExtra`] belongs to — lets the
/// frontend pick the right detail renderer without sniffing fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub enum BenchKind {
    Swe,
    Tb,
    Memory,
}

/// One bench's at-a-glance card for the cross-bench home dashboard.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct BenchInfo {
    /// Directory name under the bench root and the URL path segment
    /// (`swe`, `terminal-bench-1.0`, `terminal-bench-2.0`, `memory`).
    pub id: String,
    /// Human display label.
    pub label: String,
    pub kind: BenchKind,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub run_count: usize,
    /// ISO-8601 start of the most recent individual run, if any.
    pub last_run_at: Option<String>,
    /// Current standing (one row per arm) for the card.
    pub standing: Vec<StandingArm>,
}

/// One arm's consolidated standing — sourced from `merged-*`/`latest-*`
/// where available, else the single most recent run. Benches without an
/// arm dimension (terminal-bench) report a single arm with an empty
/// `arm` string.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct StandingArm {
    pub arm: String,
    /// Where this row came from: `"merged"`, `"latest"`, or a run id.
    pub source: String,
    pub pass_rate: f64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub n_passed: usize,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub n_total: usize,
    pub mean_f1: Option<f64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub total_cost_micro_usd: Option<i64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub input_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub output_tokens: Option<u64>,
    /// Cached (prompt-cache-hit) share of `input_tokens`. Billed cheaper
    /// and often the dominant slice. `None` when the bench didn't record it.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cached_input_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub mean_latency_ms: Option<u64>,
}

/// One individual run (one `results-*.json`) for the History lens and
/// the trend charts. `merged-*` consolidations are included with
/// `is_merged = true` so the UI can fold or flag them.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct RunSummary {
    pub bench: String,
    /// Stable per-bench key = the results filename without `.json`
    /// (e.g. `results-agent-2026-06-13__15-40-24`, `merged-agent`). The
    /// API addresses a run by this; `run_id` (in-JSON) is not unique
    /// across arms.
    pub run_key: String,
    /// The run id recorded inside the file (may repeat across arms).
    pub run_id: String,
    pub arm: Option<String>,
    pub model: Option<String>,
    pub dataset: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub n_passed: usize,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub n_total: usize,
    pub pass_rate: f64,
    pub mean_f1: Option<f64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub total_cost_micro_usd: Option<i64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub input_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub output_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cached_input_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub mean_latency_ms: Option<u64>,
    pub is_merged: bool,
}

/// One graded item: instance (swe), task/trial (tb), or question
/// (memory). The shared metrics plus a per-bench [`BenchExtra`].
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct Item {
    pub id: String,
    pub passed: bool,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub latency_ms: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub input_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub output_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cached_input_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cost_micro_usd: Option<i64>,
    /// For merged runs: which source run contributed this item.
    pub source_run: Option<String>,
    /// Relative paths (under the bench dir) to the agent trace +
    /// transcript, when present on disk. Consumed by the `/trace`
    /// endpoint (path-validated server-side).
    pub trace: Option<TracePaths>,
    /// Per-tool call counts (by `tool_name`), highest first. Derived from
    /// the agent trace on the single-run drill-in only — empty when no
    /// trace exists or in the bench/search list paths that never read it.
    pub tool_calls: Vec<ToolCount>,
    pub extra: BenchExtra,
}

/// How many times one tool was invoked in an item's agent run. Round-trips
/// through the `<trace>.tools.json` sidecar (hence `Deserialize`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct ToolCount {
    pub name: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub count: u32,
}

/// Relative paths to an item's `trace.json` + `messages.json`, under
/// the bench directory.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct TracePaths {
    pub trace: String,
    pub messages: Option<String>,
    /// Size of `trace.json` in bytes — lets the item view lazy-gate the
    /// fetch (a single terminal-bench trace has hit 166 MB; loading the
    /// full thing into the browser is the expensive part).
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub bytes: u64,
}

/// Bench-specific detail. Tagged on `type` so the frontend matches on a
/// single discriminator.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub enum BenchExtra {
    Swe {
        repo: String,
        #[cfg_attr(feature = "ts-export", ts(type = "number"))]
        patch_bytes: u64,
        empty_patch: bool,
        errored: bool,
        error: Option<String>,
        artifacts: Vec<ArtifactRef>,
    },
    Tb {
        parser_results: Vec<ParserResult>,
        failure_mode: String,
        instruction: Option<String>,
        artifacts: Vec<ArtifactRef>,
    },
    Memory {
        category: String,
        question: String,
        gold: String,
        answer: String,
        judge_reason: String,
        f1: f64,
    },
}

/// One test/parser verdict inside a terminal-bench trial
/// (`parser_results` map flattened to a list).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct ParserResult {
    pub name: String,
    pub status: String,
}

/// A pointer to a raw side-artifact (a diff, verifier stdout, asciinema
/// cast, …). `path` is relative to the bench dir; the frontend fetches
/// it via the path-validated `/file` endpoint.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct ArtifactRef {
    pub label: String,
    pub path: String,
    pub kind: ArtifactKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub enum ArtifactKind {
    /// Unified diff / patch — render with diff highlighting.
    Diff,
    /// Plain text (logs, verifier stdout).
    Text,
    /// JSON document.
    Json,
}

/// A bench's full view: card info + run history + arm standing.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct BenchDetail {
    pub info: BenchInfo,
    /// Individual runs, newest first.
    pub runs: Vec<RunSummary>,
    pub standing: Vec<StandingArm>,
}

/// A single run drilled in: its summary plus every graded item.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct RunDetail {
    pub summary: RunSummary,
    pub items: Vec<Item>,
}

/// One cross-bench search match.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../web/src/generated/"))]
pub struct SearchHit {
    pub bench: String,
    pub run_key: String,
    pub run_id: String,
    pub arm: Option<String>,
    pub item_id: String,
    pub passed: bool,
    /// What matched (repo / failure_mode / category) for context.
    pub detail: Option<String>,
}
