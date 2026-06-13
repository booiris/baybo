//! Per-bench adapters that normalize on-disk `results/` artifacts into
//! the shared [`crate::model`] spine, plus the read-side queries the API
//! serves. Everything re-scans the filesystem on call (the chosen
//! "completed-only, fresh every request" model); the artifacts are tiny
//! so globbing per request is cheap.

mod memory;
mod swe;
mod tb;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::model::*;

/// Static registry of the benches we know how to read. `id` is the
/// directory name under the bench root and the URL path segment.
pub struct BenchSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: BenchKind,
}

pub const BENCHES: &[BenchSpec] = &[
    BenchSpec {
        id: "swe",
        label: "SWE-bench",
        kind: BenchKind::Swe,
    },
    BenchSpec {
        id: "terminal-bench-1.0",
        label: "Terminal-Bench 1.0",
        kind: BenchKind::Tb,
    },
    BenchSpec {
        id: "terminal-bench-2.0",
        label: "Terminal-Bench 2.0 (Harbor)",
        kind: BenchKind::Tb,
    },
    BenchSpec {
        id: "memory",
        label: "Memory (LOCOMO)",
        kind: BenchKind::Memory,
    },
];

pub fn spec(id: &str) -> Option<&'static BenchSpec> {
    BENCHES.iter().find(|b| b.id == id)
}

/// A parsed results file: its run summary plus every graded item.
pub(crate) struct ParsedRun {
    pub summary: RunSummary,
    pub items: Vec<Item>,
}

// ── Public queries ───────────────────────────────────────────────────

/// Home dashboard: one card per bench.
pub fn scan_benches(root: &Path) -> Vec<BenchInfo> {
    BENCHES.iter().map(|s| bench_info(root, s)).collect()
}

/// A bench's full view: card + run history + arm standing.
pub fn bench_detail(root: &Path, id: &str) -> Option<BenchDetail> {
    let spec = spec(id)?;
    let mut runs: Vec<RunSummary> = parse_all(root, spec)
        .into_iter()
        .map(|p| p.summary)
        .collect();
    sort_runs_newest_first(&mut runs);
    let standing = standing_from_runs(&runs);
    let info = info_from(spec, &runs, &standing);
    Some(BenchDetail {
        info,
        runs,
        standing,
    })
}

/// One run drilled in: summary + items.
pub fn run_detail(root: &Path, id: &str, run_key: &str) -> Option<RunDetail> {
    let spec = spec(id)?;
    let path = result_files(root, spec)
        .into_iter()
        .find(|p| file_stem(p).as_deref() == Some(run_key))?;
    let parsed = parse_file(root, spec, &path, run_key).ok()?;
    // Enrich with per-tool call counts from each item's trace. Scoped to
    // this single-run view on purpose: a full run's traces can be tens of
    // MB, so the bench/search list paths never pay this.
    let bench_dir = root.join(spec.id);
    let items = parsed
        .items
        .into_iter()
        .map(|mut it| {
            if let Some(tp) = &it.trace {
                it.tool_calls = crate::trace::tool_counts(&bench_dir, &tp.trace);
            }
            it
        })
        .collect();
    Some(RunDetail {
        summary: parsed.summary,
        items,
    })
}

/// Cross-bench item search. Substring (case-insensitive) over item id,
/// the per-bench detail (repo / category / failure_mode), and the
/// `pass`/`fail` status keywords. Bounded so a huge corpus can't wedge
/// the request; the cap is logged when hit.
const SEARCH_CAP: usize = 2000;

pub fn search(root: &Path, query: &str) -> Vec<SearchHit> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for spec in BENCHES {
        for parsed in parse_all(root, spec) {
            for item in &parsed.items {
                if hits.len() >= SEARCH_CAP {
                    tracing::warn!(cap = SEARCH_CAP, "search results truncated");
                    return hits;
                }
                let detail = item_detail_text(item);
                let status_word = if item.passed { "pass" } else { "fail" };
                let hay = format!(
                    "{} {} {}",
                    item.id.to_lowercase(),
                    detail.to_lowercase(),
                    status_word
                );
                if hay.contains(&q) {
                    hits.push(SearchHit {
                        bench: spec.id.to_string(),
                        run_key: parsed.summary.run_key.clone(),
                        run_id: parsed.summary.run_id.clone(),
                        arm: parsed.summary.arm.clone(),
                        item_id: item.id.clone(),
                        passed: item.passed,
                        detail: (!detail.is_empty()).then_some(detail),
                    });
                }
            }
        }
    }
    hits
}

// ── Internals ────────────────────────────────────────────────────────

fn bench_info(root: &Path, spec: &'static BenchSpec) -> BenchInfo {
    let mut runs: Vec<RunSummary> = parse_all(root, spec)
        .into_iter()
        .map(|p| p.summary)
        .collect();
    sort_runs_newest_first(&mut runs);
    let standing = standing_from_runs(&runs);
    info_from(spec, &runs, &standing)
}

fn info_from(spec: &BenchSpec, runs: &[RunSummary], standing: &[StandingArm]) -> BenchInfo {
    let individual: Vec<&RunSummary> = runs.iter().filter(|r| !r.is_merged).collect();
    let last_run_at = individual.first().and_then(|r| r.started_at.clone());
    BenchInfo {
        id: spec.id.to_string(),
        label: spec.label.to_string(),
        kind: spec.kind,
        run_count: individual.len(),
        last_run_at,
        standing: standing.to_vec(),
    }
}

/// Parse every (non-symlink) results file for a bench. Files that fail
/// to parse are skipped with a warning rather than failing the whole
/// query.
fn parse_all(root: &Path, spec: &'static BenchSpec) -> Vec<ParsedRun> {
    let mut out = Vec::new();
    for path in result_files(root, spec) {
        let Some(run_key) = file_stem(&path) else {
            continue;
        };
        match parse_file(root, spec, &path, &run_key) {
            Ok(p) => out.push(p),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping unparseable results file")
            }
        }
    }
    out
}

fn parse_file(
    root: &Path,
    spec: &BenchSpec,
    path: &Path,
    run_key: &str,
) -> anyhow::Result<ParsedRun> {
    let bench_dir = root.join(spec.id);
    let started = mtime_iso(path);
    match spec.id {
        "swe" => swe::parse(path, &bench_dir, run_key, started),
        "memory" => memory::parse(path, &bench_dir, run_key, started),
        "terminal-bench-1.0" => tb::parse(path, &bench_dir, run_key, started, tb::Version::One),
        "terminal-bench-2.0" => tb::parse(path, &bench_dir, run_key, started, tb::Version::Two),
        other => anyhow::bail!("no adapter for bench `{other}`"),
    }
}

/// List `<root>/<id>/results/*.json`, skipping symlinks (the `latest-*`
/// pointers) so a run isn't double-counted — we compute "latest"
/// ourselves from the individual runs.
fn result_files(root: &Path, spec: &BenchSpec) -> Vec<PathBuf> {
    let dir = root.join(spec.id).join("results");
    let mut files = Vec::new();
    let Ok(reader) = std::fs::read_dir(&dir) else {
        return files;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            files.push(path);
        }
    }
    files
}

/// Build the Standing rows: one per arm. Prefer that arm's `merged-*`
/// consolidation; else its newest individual run. Benches with no arm
/// dimension collapse to a single empty-arm row.
fn standing_from_runs(runs: &[RunSummary]) -> Vec<StandingArm> {
    use std::collections::BTreeMap;
    // arm -> (merged?, newest individual)
    let mut by_arm: BTreeMap<String, (Option<&RunSummary>, Option<&RunSummary>)> = BTreeMap::new();
    for r in runs {
        let arm = r.arm.clone().unwrap_or_default();
        let slot = by_arm.entry(arm).or_insert((None, None));
        if r.is_merged {
            slot.0 = Some(r);
        } else {
            // `runs` is already newest-first, so the first individual
            // seen for an arm is its newest.
            if slot.1.is_none() {
                slot.1 = Some(r);
            }
        }
    }
    by_arm
        .into_iter()
        .filter_map(|(arm, (merged, latest))| {
            let (run, source) = match (merged, latest) {
                (Some(m), _) => (m, "merged".to_string()),
                (None, Some(l)) => (l, "latest".to_string()),
                (None, None) => return None,
            };
            Some(StandingArm {
                arm,
                source,
                pass_rate: run.pass_rate,
                n_passed: run.n_passed,
                n_total: run.n_total,
                mean_f1: run.mean_f1,
                total_cost_micro_usd: run.total_cost_micro_usd,
                input_tokens: run.input_tokens,
                output_tokens: run.output_tokens,
                cached_input_tokens: run.cached_input_tokens,
                mean_latency_ms: run.mean_latency_ms,
            })
        })
        .collect()
}

fn sort_runs_newest_first(runs: &mut [RunSummary]) {
    // mtime/metadata ISO timestamps are UTC, so lexical desc == newest
    // first. Merged (no meaningful start) sort to the end.
    runs.sort_by(|a, b| {
        b.started_at
            .clone()
            .unwrap_or_default()
            .cmp(&a.started_at.clone().unwrap_or_default())
    });
}

fn item_detail_text(item: &Item) -> String {
    match &item.extra {
        BenchExtra::Swe { repo, .. } => repo.clone(),
        BenchExtra::Tb { failure_mode, .. } => failure_mode.clone(),
        BenchExtra::Memory { category, .. } => category.clone(),
    }
}

// ── Shared path/time helpers used by the per-bench adapters ───────────

fn file_stem(path: &Path) -> Option<String> {
    path.file_stem().and_then(|s| s.to_str()).map(String::from)
}

fn mtime_iso(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: DateTime<Utc> = modified.into();
    Some(dt.to_rfc3339())
}

/// Build a [`TracePaths`] only when the trace actually exists on disk
/// (e.g. a `noop` arm has no agent → no trace). `messages` is included
/// only if its sibling exists.
pub(crate) fn trace_paths(
    bench_dir: &Path,
    trace_rel: String,
    messages_rel: String,
) -> Option<TracePaths> {
    let trace_path = bench_dir.join(&trace_rel);
    let bytes = match std::fs::metadata(&trace_path) {
        Ok(m) if m.is_file() => m.len(),
        _ => return None,
    };
    let messages = bench_dir
        .join(&messages_rel)
        .is_file()
        .then_some(messages_rel);
    Some(TracePaths {
        trace: trace_rel,
        messages,
        bytes,
    })
}

/// True when `bench_dir/rel` is an existing file.
pub(crate) fn rel_exists(bench_dir: &Path, rel: &str) -> bool {
    bench_dir.join(rel).is_file()
}

/// Milliseconds between two RFC3339 timestamps, if both parse and the
/// span is non-negative.
pub(crate) fn duration_ms(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    let s = DateTime::parse_from_rfc3339(start?).ok()?;
    let e = DateTime::parse_from_rfc3339(end?).ok()?;
    let ms = (e - s).num_milliseconds();
    (ms >= 0).then_some(ms as u64)
}
