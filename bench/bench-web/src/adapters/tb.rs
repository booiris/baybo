//! Terminal-Bench adapter (1.0 legacy `tb` + 2.0 Harbor). Trace lives
//! under `trace/<…>/agent-logs/{trace,messages}.json`, addressed by the
//! item's `trace_path` field (2.0 individual + both merged) or derived
//! from the `trial_name` timestamp (1.0 individual). Run-level model /
//! wall-clock for 1.0 comes from `runs/<ts>/run_metadata.json`. Token
//! counts (incl. cached) are recovered from the agent trace by each
//! bench's `run.sh` sync and land in the results items; the harness
//! itself records no per-task cost, so `cost_micro_usd` stays absent.

use std::path::Path;

use anyhow::Context;

use crate::adapters::{ParsedRun, duration_ms, rel_exists, trace_paths};
use crate::input::{TbRun, TbRunMetadata};
use crate::model::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Version {
    One,
    Two,
}

impl Version {
    fn bench_id(self) -> &'static str {
        match self {
            Version::One => "terminal-bench-1.0",
            Version::Two => "terminal-bench-2.0",
        }
    }
}

pub(super) fn parse(
    path: &Path,
    bench_dir: &Path,
    run_key: &str,
    mtime: Option<String>,
    version: Version,
) -> anyhow::Result<ParsedRun> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let run: TbRun =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let is_merged = run_key.starts_with("merged");

    // Run-level model / dataset / wall-clock: 1.0 only, from
    // runs/<ts>/run_metadata.json (ts is the results filename suffix).
    let mut model = None;
    let mut dataset = None;
    let mut started_at = mtime.clone();
    let mut ended_at = mtime.clone();
    let mut duration = None;
    if version == Version::One
        && !is_merged
        && let Some(ts) = run_key.strip_prefix("results-")
    {
        let meta_path = bench_dir.join(format!("runs/{ts}/run_metadata.json"));
        if let Ok(s) = std::fs::read_to_string(&meta_path)
            && let Ok(md) = serde_json::from_str::<TbRunMetadata>(&s)
        {
            model = md.model_name.clone();
            dataset = md.dataset_name.clone();
            if md.start_time.is_some() {
                started_at = md.start_time.clone();
            }
            if md.end_time.is_some() {
                ended_at = md.end_time.clone();
            }
            duration = duration_ms(md.start_time.as_deref(), md.end_time.as_deref());
        }
    }

    let items = run
        .results
        .iter()
        .map(|it| {
            let trace_rel = it
                .trace_path
                .as_ref()
                .map(|p| format!("trace/{p}"))
                .or_else(|| tb1_trace_from_trial(&it.trial_name, &it.task_id));
            let trace = trace_rel.and_then(|tr| {
                let messages = tr.replace("trace.json", "messages.json");
                trace_paths(bench_dir, tr, messages)
            });

            let mut artifacts = Vec::new();
            if version == Version::Two {
                // Harbor verifier output: runs/<ts>/<trial_name>/verifier/*
                let stdout_rel =
                    format!("runs/{}/{}/verifier/test-stdout.txt", run.id, it.trial_name);
                if rel_exists(bench_dir, &stdout_rel) {
                    artifacts.push(ArtifactRef {
                        label: "verifier stdout".to_string(),
                        path: stdout_rel,
                        kind: ArtifactKind::Text,
                    });
                }
                let ctrf_rel = format!("runs/{}/{}/verifier/ctrf.json", run.id, it.trial_name);
                if rel_exists(bench_dir, &ctrf_rel) {
                    artifacts.push(ArtifactRef {
                        label: "verifier report (ctrf)".to_string(),
                        path: ctrf_rel,
                        kind: ArtifactKind::Json,
                    });
                }
            }
            if let Some(rec) = &it.recording_path {
                let rec_rel = format!("runs/{rec}");
                if rel_exists(bench_dir, &rec_rel) {
                    artifacts.push(ArtifactRef {
                        label: "agent recording (asciinema cast)".to_string(),
                        path: rec_rel,
                        kind: ArtifactKind::Text,
                    });
                }
            }

            let parser_results = it
                .parser_results
                .iter()
                .map(|(name, status)| ParserResult {
                    name: name.clone(),
                    status: status.clone(),
                })
                .collect();

            Item {
                id: it.task_id.clone(),
                passed: it.is_resolved,
                latency_ms: duration_ms(
                    it.trial_started_at.as_deref(),
                    it.trial_ended_at.as_deref(),
                ),
                input_tokens: (it.total_input_tokens > 0).then_some(it.total_input_tokens),
                output_tokens: (it.total_output_tokens > 0).then_some(it.total_output_tokens),
                cached_input_tokens: (it.total_cached_input_tokens > 0)
                    .then_some(it.total_cached_input_tokens),
                // The terminal-bench harness records no per-task cost.
                cost_micro_usd: None,
                source_run: it.source_run.clone(),
                trace,
                tool_calls: Vec::new(),
                extra: BenchExtra::Tb {
                    parser_results,
                    failure_mode: it.failure_mode.clone(),
                    instruction: it.instruction.clone(),
                    artifacts,
                },
            }
        })
        .collect::<Vec<_>>();

    let n_total = run.results.len();
    let n_passed = run.results.iter().filter(|r| r.is_resolved).count();
    let pass_rate = if n_total > 0 {
        n_passed as f64 / n_total as f64
    } else {
        0.0
    };
    let input_tokens: u64 = run.results.iter().map(|r| r.total_input_tokens).sum();
    let output_tokens: u64 = run.results.iter().map(|r| r.total_output_tokens).sum();
    let cached_input_tokens: u64 = run
        .results
        .iter()
        .map(|r| r.total_cached_input_tokens)
        .sum();

    // run_id: 1.0's in-JSON `id` is a UUID, but the timestamp (filename
    // suffix) keys the trace dirs + run_metadata, so prefer it; 2.0's
    // `id` already is the timestamp.
    let run_id = match (version, is_merged) {
        (Version::One, false) => run_key
            .strip_prefix("results-")
            .map(String::from)
            .unwrap_or_else(|| run.id.clone()),
        _ if run.id.is_empty() => run_key.to_string(),
        _ => run.id.clone(),
    };

    let summary = RunSummary {
        bench: version.bench_id().to_string(),
        run_key: run_key.to_string(),
        run_id,
        arm: None,
        model,
        dataset,
        started_at,
        ended_at,
        duration_ms: duration,
        n_passed,
        n_total,
        pass_rate,
        mean_f1: None,
        total_cost_micro_usd: None,
        input_tokens: (input_tokens > 0).then_some(input_tokens),
        output_tokens: (output_tokens > 0).then_some(output_tokens),
        cached_input_tokens: (cached_input_tokens > 0).then_some(cached_input_tokens),
        mean_latency_ms: None,
        is_merged,
    };

    Ok(ParsedRun { summary, items })
}

/// terminal-bench 1.0 individual runs carry no `trace_path`; derive it
/// from the `trial_name` (its last dot-segment is the run timestamp):
/// `trace/<ts>/<task_id>/<trial_name>/agent-logs/trace.json`.
fn tb1_trace_from_trial(trial_name: &str, task_id: &str) -> Option<String> {
    let ts = trial_name.rsplit('.').next()?;
    if ts.is_empty() || ts == trial_name {
        return None;
    }
    Some(format!(
        "trace/{ts}/{task_id}/{trial_name}/agent-logs/trace.json"
    ))
}
