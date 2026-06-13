//! Per-question result records, per-category aggregation, and the summary
//! table.

use std::collections::BTreeMap;

use serde::Serialize;

/// Divisor to render integer micro-USD as dollars at print time only.
const MICRO_USD_PER_USD: f64 = 1_000_000.0;

#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub conv_idx: usize,
    /// The test set's own category label (e.g. LOCOMO's "multi-hop").
    pub category: String,
    pub question: String,
    pub gold: String,
    pub answer: String,
    pub correct: bool,
    pub f1: f64,
    pub judge_reason: String,
    pub latency_ms: u64,
    pub session_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cached (prompt-cache-hit) share of `input_tokens`.
    pub cached_input_tokens: u64,
    pub cost_micro_usd: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryStat {
    pub category: String,
    pub total: usize,
    pub correct: usize,
    pub accuracy: f64,
    pub mean_f1: f64,
    pub mean_latency_ms: f64,
    pub mean_input_tokens: f64,
    pub mean_output_tokens: f64,
    pub mean_cost_micro_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub testset: String,
    pub arm: String,
    pub answer_model: String,
    pub judge_model: String,
    pub conversations: usize,
    pub total_questions: usize,
    pub total_correct: usize,
    pub overall_accuracy: f64,
    pub mean_f1: f64,
    pub mean_latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub total_cost_micro_usd: i64,
    pub mean_input_tokens: f64,
    pub mean_output_tokens: f64,
    pub mean_cost_micro_usd: f64,
    pub by_category: Vec<CategoryStat>,
    pub results: Vec<QuestionResult>,
}

/// Metadata describing a single arm's run, bundled to keep [`aggregate`]'s
/// signature small.
pub struct ReportMeta {
    pub run_id: String,
    pub testset: String,
    pub arm: String,
    pub answer_model: String,
    pub judge_model: String,
    pub conversations: usize,
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, count) = values.fold((0.0, 0usize), |(s, c), x| (s + x, c + 1));
    if count == 0 { 0.0 } else { sum / count as f64 }
}

pub fn aggregate(meta: ReportMeta, results: Vec<QuestionResult>) -> RunReport {
    let total_questions = results.len();
    let total_correct = results.iter().filter(|r| r.correct).count();
    let mean_latency_ms = if total_questions == 0 {
        0
    } else {
        results.iter().map(|r| r.latency_ms).sum::<u64>() / total_questions as u64
    };

    let input_tokens: u64 = results.iter().map(|r| r.input_tokens).sum();
    let output_tokens: u64 = results.iter().map(|r| r.output_tokens).sum();
    let cached_input_tokens: u64 = results.iter().map(|r| r.cached_input_tokens).sum();
    let total_cost_micro_usd: i64 = results.iter().map(|r| r.cost_micro_usd).sum();
    let per_question = |total: f64| {
        if total_questions == 0 {
            0.0
        } else {
            total / total_questions as f64
        }
    };
    let mean_input_tokens = per_question(input_tokens as f64);
    let mean_output_tokens = per_question(output_tokens as f64);
    let mean_cost_micro_usd = per_question(total_cost_micro_usd as f64);

    let mut buckets: BTreeMap<String, Vec<&QuestionResult>> = BTreeMap::new();
    for r in &results {
        buckets.entry(r.category.clone()).or_default().push(r);
    }
    let by_category = buckets
        .into_iter()
        .map(|(category, rs)| {
            let total = rs.len();
            let correct = rs.iter().filter(|r| r.correct).count();
            CategoryStat {
                category,
                total,
                correct,
                accuracy: ratio(correct, total),
                mean_f1: mean(rs.iter().map(|r| r.f1)),
                mean_latency_ms: mean(rs.iter().map(|r| r.latency_ms as f64)),
                mean_input_tokens: mean(rs.iter().map(|r| r.input_tokens as f64)),
                mean_output_tokens: mean(rs.iter().map(|r| r.output_tokens as f64)),
                mean_cost_micro_usd: mean(rs.iter().map(|r| r.cost_micro_usd as f64)),
            }
        })
        .collect();

    RunReport {
        run_id: meta.run_id,
        testset: meta.testset,
        arm: meta.arm,
        answer_model: meta.answer_model,
        judge_model: meta.judge_model,
        conversations: meta.conversations,
        total_questions,
        total_correct,
        overall_accuracy: ratio(total_correct, total_questions),
        mean_f1: mean(results.iter().map(|r| r.f1)),
        mean_latency_ms,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        total_cost_micro_usd,
        mean_input_tokens,
        mean_output_tokens,
        mean_cost_micro_usd,
        by_category,
        results,
    }
}

/// Print the human-readable summary table (per-category accuracy + overall).
pub fn print_table(report: &RunReport) {
    println!(
        "\n=== memory benchmark — testset: {}  arm: {} ===",
        report.testset, report.arm
    );
    println!(
        "run_id={}  answer={}  judge={}",
        report.run_id, report.answer_model, report.judge_model
    );
    println!(
        "conversations={}  questions={}",
        report.conversations, report.total_questions
    );
    println!(
        "{:<14} {:>5} {:>7} {:>8} {:>9} {:>8} {:>8} {:>10}",
        "category", "n", "acc", "f1", "lat_ms", "in_tok", "out_tok", "cost$"
    );
    for c in &report.by_category {
        println!(
            "{:<14} {:>5} {:>6.1}% {:>8.3} {:>9.0} {:>8.0} {:>8.0} {:>10.6}",
            c.category,
            c.total,
            c.accuracy * 100.0,
            c.mean_f1,
            c.mean_latency_ms,
            c.mean_input_tokens,
            c.mean_output_tokens,
            c.mean_cost_micro_usd / MICRO_USD_PER_USD,
        );
    }
    println!(
        "{:<14} {:>5} {:>6.1}% {:>8.3} {:>9} {:>8.0} {:>8.0} {:>10.6}",
        "OVERALL",
        report.total_questions,
        report.overall_accuracy * 100.0,
        report.mean_f1,
        report.mean_latency_ms,
        report.mean_input_tokens,
        report.mean_output_tokens,
        report.mean_cost_micro_usd / MICRO_USD_PER_USD,
    );
    println!(
        "totals: in_tok={} out_tok={} spend=${:.6}",
        report.input_tokens,
        report.output_tokens,
        report.total_cost_micro_usd as f64 / MICRO_USD_PER_USD,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(
        category: &str,
        latency_ms: u64,
        input: u64,
        output: u64,
        cost: i64,
    ) -> QuestionResult {
        QuestionResult {
            conv_idx: 0,
            category: category.to_string(),
            question: "q".to_string(),
            gold: "g".to_string(),
            answer: "a".to_string(),
            correct: true,
            f1: 1.0,
            judge_reason: "r".to_string(),
            latency_ms,
            session_id: "s".to_string(),
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: 0,
            cost_micro_usd: cost,
        }
    }

    fn meta() -> ReportMeta {
        ReportMeta {
            run_id: "run".to_string(),
            testset: "locomo".to_string(),
            arm: "noop".to_string(),
            answer_model: "answer".to_string(),
            judge_model: "judge".to_string(),
            conversations: 1,
        }
    }

    #[test]
    fn aggregate_sums_tokens_and_cost() {
        let results = vec![
            result("a", 100, 10, 2, 1000),
            result("a", 200, 20, 4, 3000),
            result("b", 300, 30, 6, 5000),
        ];
        let report = aggregate(meta(), results);

        assert_eq!(report.input_tokens, 60);
        assert_eq!(report.output_tokens, 12);
        assert_eq!(report.total_cost_micro_usd, 9000);
        assert_eq!(report.mean_input_tokens, 20.0);
        assert_eq!(report.mean_output_tokens, 4.0);
        assert_eq!(report.mean_cost_micro_usd, 3000.0);
        assert_eq!(report.mean_latency_ms, 200);

        let cat_a = report
            .by_category
            .iter()
            .find(|c| c.category == "a")
            .expect("category a");
        assert_eq!(cat_a.mean_latency_ms, 150.0);
        assert_eq!(cat_a.mean_input_tokens, 15.0);
        assert_eq!(cat_a.mean_output_tokens, 3.0);
        assert_eq!(cat_a.mean_cost_micro_usd, 2000.0);

        let cat_b = report
            .by_category
            .iter()
            .find(|c| c.category == "b")
            .expect("category b");
        assert_eq!(cat_b.mean_cost_micro_usd, 5000.0);
    }

    #[test]
    fn aggregate_empty_results_is_zeroed() {
        let report = aggregate(meta(), Vec::new());
        assert_eq!(report.total_questions, 0);
        assert_eq!(report.input_tokens, 0);
        assert_eq!(report.output_tokens, 0);
        assert_eq!(report.total_cost_micro_usd, 0);
        assert_eq!(report.mean_input_tokens, 0.0);
        assert_eq!(report.mean_output_tokens, 0.0);
        assert_eq!(report.mean_cost_micro_usd, 0.0);
        assert_eq!(report.mean_latency_ms, 0);
        assert!(report.by_category.is_empty());
    }
}
