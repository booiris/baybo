//! Per-instance result records, per-repo aggregation, and the summary table.

use std::collections::BTreeMap;

use serde::Serialize;

/// Divisor to render integer micro-USD as dollars at print time only.
const MICRO_USD_PER_USD: f64 = 1_000_000.0;

/// One graded instance: the harness verdict joined with the agent run's metrics.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceResult {
    pub instance_id: String,
    pub repo: String,
    pub resolved: bool,
    pub empty_patch: bool,
    pub errored: bool,
    pub patch_bytes: usize,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoStat {
    pub repo: String,
    pub total: usize,
    pub resolved: usize,
    pub resolved_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub dataset: String,
    pub split: String,
    pub arm: String,
    pub model: String,
    pub total_instances: usize,
    pub resolved: usize,
    pub empty_patches: usize,
    pub errored: usize,
    pub resolved_rate: f64,
    pub mean_latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_micro_usd: i64,
    pub mean_input_tokens: f64,
    pub mean_output_tokens: f64,
    pub mean_cost_micro_usd: f64,
    pub by_repo: Vec<RepoStat>,
    pub results: Vec<InstanceResult>,
}

/// Run metadata, bundled to keep [`aggregate`]'s signature small.
pub struct ReportMeta {
    pub run_id: String,
    pub dataset: String,
    pub split: String,
    pub arm: String,
    pub model: String,
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

pub fn aggregate(meta: ReportMeta, results: Vec<InstanceResult>) -> RunReport {
    let total_instances = results.len();
    let resolved = results.iter().filter(|r| r.resolved).count();
    let empty_patches = results.iter().filter(|r| r.empty_patch).count();
    let errored = results.iter().filter(|r| r.errored).count();
    let mean_latency_ms = if total_instances == 0 {
        0
    } else {
        results.iter().map(|r| r.latency_ms).sum::<u64>() / total_instances as u64
    };

    let input_tokens: u64 = results.iter().map(|r| r.input_tokens).sum();
    let output_tokens: u64 = results.iter().map(|r| r.output_tokens).sum();
    let total_cost_micro_usd: i64 = results.iter().map(|r| r.cost_micro_usd).sum();
    let per_instance = |total: f64| {
        if total_instances == 0 {
            0.0
        } else {
            total / total_instances as f64
        }
    };

    let mut buckets: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in &results {
        let entry = buckets.entry(r.repo.clone()).or_insert((0, 0));
        entry.0 += 1;
        if r.resolved {
            entry.1 += 1;
        }
    }
    let by_repo = buckets
        .into_iter()
        .map(|(repo, (total, resolved))| RepoStat {
            repo,
            total,
            resolved,
            resolved_rate: ratio(resolved, total),
        })
        .collect();

    RunReport {
        run_id: meta.run_id,
        dataset: meta.dataset,
        split: meta.split,
        arm: meta.arm,
        model: meta.model,
        total_instances,
        resolved,
        empty_patches,
        errored,
        resolved_rate: ratio(resolved, total_instances),
        mean_latency_ms,
        input_tokens,
        output_tokens,
        total_cost_micro_usd,
        mean_input_tokens: per_instance(input_tokens as f64),
        mean_output_tokens: per_instance(output_tokens as f64),
        mean_cost_micro_usd: per_instance(total_cost_micro_usd as f64),
        by_repo,
        results,
    }
}

/// Print the human-readable summary (per-repo resolved-rate + overall).
pub fn print_table(report: &RunReport) {
    println!(
        "\n=== SWE-bench — dataset: {} ({})  arm: {} ===",
        report.dataset, report.split, report.arm
    );
    println!("run_id={}  model={}", report.run_id, report.model);
    println!("{:<40} {:>5} {:>9} {:>8}", "repo", "n", "resolved", "rate");
    for r in &report.by_repo {
        println!(
            "{:<40} {:>5} {:>9} {:>7.1}%",
            r.repo,
            r.total,
            r.resolved,
            r.resolved_rate * 100.0,
        );
    }
    println!(
        "{:<40} {:>5} {:>9} {:>7.1}%",
        "OVERALL",
        report.total_instances,
        report.resolved,
        report.resolved_rate * 100.0,
    );
    println!(
        "empty_patches={}  errored={}  mean_lat_ms={}",
        report.empty_patches, report.errored, report.mean_latency_ms
    );
    println!(
        "tokens: in={} out={}  spend=${:.6}  (mean ${:.6}/instance)",
        report.input_tokens,
        report.output_tokens,
        report.total_cost_micro_usd as f64 / MICRO_USD_PER_USD,
        report.mean_cost_micro_usd / MICRO_USD_PER_USD,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(instance_id: &str, repo: &str, resolved: bool, cost: i64) -> InstanceResult {
        InstanceResult {
            instance_id: instance_id.to_string(),
            repo: repo.to_string(),
            resolved,
            empty_patch: false,
            errored: false,
            patch_bytes: 10,
            latency_ms: 100,
            input_tokens: 50,
            output_tokens: 10,
            cost_micro_usd: cost,
            error: None,
        }
    }

    fn meta() -> ReportMeta {
        ReportMeta {
            run_id: "run".to_string(),
            dataset: "SWE-bench_Lite".to_string(),
            split: "test".to_string(),
            arm: "agent".to_string(),
            model: "aura".to_string(),
        }
    }

    #[test]
    fn aggregate_computes_resolved_rate_and_per_repo() {
        let results = vec![
            result("django__django-1", "django/django", true, 1000),
            result("django__django-2", "django/django", false, 2000),
            result("sympy__sympy-3", "sympy/sympy", true, 3000),
        ];
        let report = aggregate(meta(), results);
        assert_eq!(report.total_instances, 3);
        assert_eq!(report.resolved, 2);
        assert!((report.resolved_rate - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(report.total_cost_micro_usd, 6000);
        assert_eq!(report.mean_cost_micro_usd, 2000.0);

        let django = report
            .by_repo
            .iter()
            .find(|r| r.repo == "django/django")
            .expect("django repo");
        assert_eq!(django.total, 2);
        assert_eq!(django.resolved, 1);
        assert_eq!(django.resolved_rate, 0.5);
    }

    #[test]
    fn aggregate_empty_is_zeroed() {
        let report = aggregate(meta(), Vec::new());
        assert_eq!(report.total_instances, 0);
        assert_eq!(report.resolved, 0);
        assert_eq!(report.resolved_rate, 0.0);
        assert_eq!(report.mean_cost_micro_usd, 0.0);
        assert!(report.by_repo.is_empty());
    }
}
