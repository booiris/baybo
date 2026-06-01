//! Per-question result records, per-category aggregation, and the summary
//! table.

use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub conv_idx: usize,
    /// The test set's own category label (e.g. LOCOMO's "multi-hop").
    pub category: String,
    pub question: String,
    pub gold: String,
    pub answer: String,
    pub correct: bool,
    pub adversarial: bool,
    pub f1: f64,
    pub judge_reason: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryStat {
    pub category: String,
    pub total: usize,
    pub correct: usize,
    pub accuracy: f64,
    pub mean_f1: f64,
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
    pub tokens: (u64, u64),
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
        input_tokens: meta.tokens.0,
        output_tokens: meta.tokens.1,
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
    println!("{:<14} {:>5} {:>8} {:>8}", "category", "n", "acc", "f1");
    for c in &report.by_category {
        println!(
            "{:<14} {:>5} {:>7.1}% {:>8.3}",
            c.category,
            c.total,
            c.accuracy * 100.0,
            c.mean_f1
        );
    }
    println!(
        "{:<14} {:>5} {:>7.1}% {:>8.3}",
        "OVERALL",
        report.total_questions,
        report.overall_accuracy * 100.0,
        report.mean_f1
    );
    println!(
        "tokens: in={} out={}   mean_latency={}ms",
        report.input_tokens, report.output_tokens, report.mean_latency_ms
    );
}
