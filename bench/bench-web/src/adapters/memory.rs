//! Memory / LOCOMO adapter. Trace layout:
//! `trace/<run_id>/<arm>/<session_id>.{trace,messages}.json` (run_id is
//! `merged` for consolidations). The per-question session_id is both the
//! item id and the trace key. No runs exist on disk yet; this adapter is
//! written against `bench/memory/src/report.rs` and exercised once a
//! memory run lands.

use std::path::Path;

use anyhow::Context;

use crate::adapters::{ParsedRun, trace_paths};
use crate::input::MemoryRun;
use crate::model::*;

pub(super) fn parse(
    path: &Path,
    bench_dir: &Path,
    run_key: &str,
    started_at: Option<String>,
) -> anyhow::Result<ParsedRun> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let run: MemoryRun =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let is_merged = run_key.starts_with("merged");

    let items = run
        .results
        .iter()
        .enumerate()
        .map(|(idx, it)| {
            let id = if it.session_id.is_empty() {
                format!("conv{}-q{}", it.conv_idx, idx)
            } else {
                it.session_id.clone()
            };
            let trace = if it.session_id.is_empty() {
                None
            } else {
                trace_paths(
                    bench_dir,
                    format!(
                        "trace/{}/{}/{}.trace.json",
                        run.run_id, run.arm, it.session_id
                    ),
                    format!(
                        "trace/{}/{}/{}.messages.json",
                        run.run_id, run.arm, it.session_id
                    ),
                )
            };
            Item {
                id,
                passed: it.correct,
                latency_ms: Some(it.latency_ms),
                input_tokens: Some(it.input_tokens),
                output_tokens: Some(it.output_tokens),
                cached_input_tokens: Some(it.cached_input_tokens),
                cost_micro_usd: Some(it.cost_micro_usd),
                source_run: it.source_run.clone(),
                trace,
                extra: BenchExtra::Memory {
                    category: it.category.clone(),
                    question: it.question.clone(),
                    gold: it.gold.clone(),
                    answer: it.answer.clone(),
                    judge_reason: it.judge_reason.clone(),
                    f1: it.f1,
                },
            }
        })
        .collect::<Vec<_>>();

    let n_total = run.results.len();
    let n_passed = run.results.iter().filter(|r| r.correct).count();

    let summary = RunSummary {
        bench: "memory".to_string(),
        run_key: run_key.to_string(),
        run_id: run.run_id.clone(),
        arm: Some(run.arm.clone()),
        model: run.answer_model.clone(),
        dataset: run.testset.clone(),
        started_at: started_at.clone(),
        ended_at: started_at,
        duration_ms: None,
        n_passed,
        n_total,
        pass_rate: run.overall_accuracy,
        mean_f1: Some(run.mean_f1),
        total_cost_micro_usd: Some(run.total_cost_micro_usd),
        input_tokens: Some(run.input_tokens),
        output_tokens: Some(run.output_tokens),
        cached_input_tokens: Some(run.cached_input_tokens),
        mean_latency_ms: Some(run.mean_latency_ms.round() as u64),
        is_merged,
    };

    Ok(ParsedRun { summary, items })
}
