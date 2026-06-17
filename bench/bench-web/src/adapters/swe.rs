//! SWE-bench adapter. Trace layout:
//! `trace/<run_id>/agent/<instance_id>.{trace,messages}.json`
//! (run_id is literally `merged` for the consolidations). The per-
//! instance patch lives in `runs/predictions-<arm>-<run_id>.jsonl`,
//! surfaced as a `Diff` artifact addressed by a `<file>#<instance>`
//! fragment the `/file` endpoint extracts.

use std::path::Path;

use anyhow::Context;

use crate::adapters::{ParsedRun, rel_exists, trace_paths};
use crate::input::SweRun;
use crate::model::*;

pub(super) fn parse(
    path: &Path,
    bench_dir: &Path,
    run_key: &str,
    started_at: Option<String>,
) -> anyhow::Result<ParsedRun> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let run: SweRun =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let is_merged = run_key.starts_with("merged");

    let predictions_rel = format!("runs/predictions-{}-{}.jsonl", run.arm, run.run_id);
    let predictions_exist = rel_exists(bench_dir, &predictions_rel);

    let items = run
        .results
        .iter()
        .map(|it| {
            // For merged rows the trace was mirrored under trace/merged/;
            // source_run is informational. Use the file's run_id (which
            // is `merged` for consolidations) as the trace directory.
            let trace = trace_paths(
                bench_dir,
                format!("trace/{}/agent/{}.trace.json", run.run_id, it.instance_id),
                format!(
                    "trace/{}/agent/{}.messages.json",
                    run.run_id, it.instance_id
                ),
            );
            let mut artifacts = Vec::new();
            if predictions_exist && !it.empty_patch {
                artifacts.push(ArtifactRef {
                    label: "patch (diff)".to_string(),
                    path: format!("{predictions_rel}#{}", it.instance_id),
                    kind: ArtifactKind::Diff,
                });
            }
            Item {
                id: it.instance_id.clone(),
                passed: it.resolved,
                latency_ms: Some(it.latency_ms),
                input_tokens: Some(it.input_tokens),
                output_tokens: Some(it.output_tokens),
                cached_input_tokens: Some(it.cached_input_tokens),
                cost_micro_usd: Some(it.cost_micro_usd),
                source_run: it.source_run.clone(),
                trace,
                tool_calls: Vec::new(),
                extra: BenchExtra::Swe {
                    repo: it.repo.clone(),
                    patch_bytes: it.patch_bytes,
                    empty_patch: it.empty_patch,
                    errored: it.errored,
                    error: it.error.clone(),
                    failure_reason: it.failure_reason.clone(),
                    artifacts,
                },
            }
        })
        .collect::<Vec<_>>();

    let n_total = run.results.len();
    let n_passed = run.results.iter().filter(|r| r.resolved).count();
    let pass_rate = if n_total > 0 {
        n_passed as f64 / n_total as f64
    } else {
        0.0
    };

    let summary = RunSummary {
        bench: "swe".to_string(),
        run_key: run_key.to_string(),
        run_id: run.run_id.clone(),
        arm: Some(run.arm.clone()),
        model: run.model.clone(),
        dataset: run.dataset.clone(),
        started_at: started_at.clone(),
        // SWE results carry no explicit wall-clock; the file mtime is the
        // completion stamp, and mean_latency_ms covers per-instance time.
        ended_at: started_at,
        duration_ms: None,
        n_passed,
        n_total,
        pass_rate,
        mean_f1: None,
        total_cost_micro_usd: Some(run.total_cost_micro_usd),
        input_tokens: Some(run.input_tokens),
        output_tokens: Some(run.output_tokens),
        cached_input_tokens: Some(run.cached_input_tokens),
        mean_latency_ms: Some(run.mean_latency_ms.round() as u64),
        is_merged,
    };

    Ok(ParsedRun { summary, items })
}
