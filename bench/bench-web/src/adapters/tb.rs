//! Terminal-Bench 2.0 (Harbor) adapter. Trace lives under
//! `trace/<…>/agent-logs/{trace,messages}.json`, addressed by the item's
//! `trace_path` field. Token counts (incl. cached) are recovered from the
//! agent trace by the bench's `run.sh` sync and land in the results
//! items; the harness itself records no per-task cost, so
//! `cost_micro_usd` stays absent.

use std::path::Path;

use anyhow::Context;

use crate::adapters::{ParsedRun, duration_ms, rel_exists, trace_paths};
use crate::input::TbRun;
use crate::model::*;

pub(super) fn parse(
    path: &Path,
    bench_dir: &Path,
    run_key: &str,
    mtime: Option<String>,
) -> anyhow::Result<ParsedRun> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let run: TbRun =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let is_merged = run_key.starts_with("merged");

    let items = run
        .results
        .iter()
        .map(|it| {
            let trace_rel = it.trace_path.as_ref().map(|p| format!("trace/{p}"));
            let trace = trace_rel.and_then(|tr| {
                let messages = tr.replace("trace.json", "messages.json");
                trace_paths(bench_dir, tr, messages)
            });

            // Harbor verifier output: runs/<ts>/<trial_name>/verifier/*
            let mut artifacts = Vec::new();
            let stdout_rel = format!("runs/{}/{}/verifier/test-stdout.txt", run.id, it.trial_name);
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

    let run_id = if run.id.is_empty() {
        run_key.to_string()
    } else {
        run.id.clone()
    };

    let summary = RunSummary {
        bench: "terminal-bench-2.0".to_string(),
        run_key: run_key.to_string(),
        run_id,
        arm: None,
        model: None,
        dataset: None,
        started_at: mtime.clone(),
        ended_at: mtime,
        duration_ms: None,
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
