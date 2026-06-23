//! End-to-end adapter + API tests over a synthetic bench tree built in a
//! TempDir — deterministic and CI-safe (the real `bench/*` artifacts are
//! gitignored and absent on a fresh clone). Locks in the normalization
//! spine plus the two bugs caught during the build: the `merged-*` float
//! `mean_latency_ms`, and the `latest-*` symlink dedup.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use baybo_bench_web::adapters;
use baybo_bench_web::model::BenchExtra;

// ── Fixtures ─────────────────────────────────────────────────────────

const SWE_AGENT_RUN: &str = r#"{
  "run_id": "2026-01-01__00-00-00", "dataset": "SWE-bench_Lite", "arm": "agent", "model": "baybo",
  "mean_latency_ms": 100, "input_tokens": 50, "output_tokens": 10, "total_cost_micro_usd": 4840,
  "results": [{"instance_id": "repo__pkg-1", "repo": "repo/pkg", "resolved": true,
    "empty_patch": false, "errored": false, "patch_bytes": 120, "latency_ms": 100,
    "input_tokens": 50, "output_tokens": 10, "cost_micro_usd": 4840}]
}"#;

// `mean_latency_ms` is a FLOAT here (the cross-run merged consolidation
// writes a fractional mean). A u64 DTO silently dropped every merged
// file before this was fixed — this fixture is the regression guard.
const SWE_MERGED_AGENT: &str = r#"{
  "run_id": "merged", "arm": "agent", "mean_latency_ms": 182740.5,
  "input_tokens": 50, "output_tokens": 10, "total_cost_micro_usd": 4840,
  "source_runs": ["2026-01-01__00-00-00"],
  "results": [{"instance_id": "repo__pkg-1", "repo": "repo/pkg", "resolved": true,
    "patch_bytes": 120, "latency_ms": 100, "cost_micro_usd": 4840,
    "source_run": "2026-01-01__00-00-00"}]
}"#;

// Diagnostic arms (oracle = ceiling, noop = floor). The viewer hides these
// for SWE so only `agent` shows — see `swe_hides_diagnostic_arms`.
const SWE_ORACLE_RUN: &str = r#"{
  "run_id": "2026-01-01__00-00-00", "arm": "oracle", "model": "gold", "mean_latency_ms": 0,
  "results": [{"instance_id": "repo__pkg-1", "repo": "repo/pkg", "resolved": true}]
}"#;
const SWE_NOOP_RUN: &str = r#"{
  "run_id": "2026-01-01__00-00-00", "arm": "noop", "model": "baybo-noop", "mean_latency_ms": 0,
  "results": [{"instance_id": "repo__pkg-1", "repo": "repo/pkg", "resolved": false, "empty_patch": true}]
}"#;

const TB1_RUN: &str = r#"{
  "id": "uuid-1", "results": [{"task_id": "mytask",
    "trial_name": "mytask.1-of-1.2026-02-02__00-00-00", "is_resolved": true,
    "failure_mode": "unset", "parser_results": {"tests::a": "passed"}}]
}"#;

const TB1_META: &str = r#"{"model_name": "deepseek", "dataset_name": "core",
  "start_time": "2026-02-02T00:00:00Z", "end_time": "2026-02-02T00:05:00Z"}"#;

const TB2_RUN: &str = r#"{
  "id": "2026-03-03__00-00-00", "results": [{"task_id": "t2", "trial_name": "t2__abc",
    "is_resolved": false, "failure_mode": "unset", "parser_results": {},
    "trace_path": "2026-03-03__00-00-00/t2/t2__abc/agent-logs/trace.json"}]
}"#;

const TRACE_JSON: &str =
    r#"{"session": "s1", "jobs": [{"job": {"id": "j1", "session_id": "s1"}, "steps": []}]}"#;

const MESSAGES_JSON: &str = r#"{"session": "s1", "messages": [{"ordinal": 0, "superseded_by": null,
  "created_at": "2026-01-01T00:00:00Z",
  "message": {"role": "user", "source": "user", "content": [{"Text": "hi"}]}}]}"#;

const PREDICTIONS: &str = "{\"instance_id\": \"repo__pkg-1\", \"model_name_or_path\": \"baybo\", \"model_patch\": \"diff --git a/x b/x\\n+hi\\n\"}\n";

fn write(path: PathBuf, contents: &str) {
    fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

/// Build a synthetic bench root with swe (individual + merged + a
/// `latest-*` symlink), terminal-bench-1.0, and terminal-bench-2.0.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let r = dir.path();

    // SWE
    write(
        r.join("swe/results/results-agent-2026-01-01__00-00-00.json"),
        SWE_AGENT_RUN,
    );
    write(r.join("swe/results/merged-agent.json"), SWE_MERGED_AGENT);
    // Diagnostic arms present on disk but expected to be filtered out.
    write(
        r.join("swe/results/results-oracle-2026-01-01__00-00-00.json"),
        SWE_ORACLE_RUN,
    );
    write(r.join("swe/results/merged-oracle.json"), SWE_ORACLE_RUN);
    write(
        r.join("swe/results/results-noop-2026-01-01__00-00-00.json"),
        SWE_NOOP_RUN,
    );
    symlink(
        "results-agent-2026-01-01__00-00-00.json",
        r.join("swe/results/latest-agent.json"),
    )
    .expect("symlink");
    write(
        r.join("swe/trace/2026-01-01__00-00-00/agent/repo__pkg-1.trace.json"),
        TRACE_JSON,
    );
    write(
        r.join("swe/trace/2026-01-01__00-00-00/agent/repo__pkg-1.messages.json"),
        MESSAGES_JSON,
    );
    write(
        r.join("swe/runs/predictions-agent-2026-01-01__00-00-00.jsonl"),
        PREDICTIONS,
    );

    // terminal-bench 1.0 (trace path derived from trial_name)
    write(
        r.join("terminal-bench-1.0/results/results-2026-02-02__00-00-00.json"),
        TB1_RUN,
    );
    write(
        r.join("terminal-bench-1.0/runs/2026-02-02__00-00-00/run_metadata.json"),
        TB1_META,
    );
    write(
        r.join("terminal-bench-1.0/trace/2026-02-02__00-00-00/mytask/mytask.1-of-1.2026-02-02__00-00-00/agent-logs/trace.json"),
        TRACE_JSON,
    );

    // terminal-bench 2.0 (trace path from the `trace_path` field)
    write(
        r.join("terminal-bench-2.0/results/results-2026-03-03__00-00-00.json"),
        TB2_RUN,
    );
    write(
        r.join("terminal-bench-2.0/trace/2026-03-03__00-00-00/t2/t2__abc/agent-logs/trace.json"),
        TRACE_JSON,
    );

    dir
}

fn root(dir: &tempfile::TempDir) -> &Path {
    dir.path()
}

// ── Adapter tests ────────────────────────────────────────────────────

#[test]
fn scan_lists_all_registered_benches() {
    let d = fixture();
    let benches = adapters::scan_benches(root(&d));
    let ids: Vec<_> = benches.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(
        ids,
        ["swe", "terminal-bench-1.0", "terminal-bench-2.0", "memory"]
    );
    // memory has no runs in the fixture → empty, not a panic.
    let mem = benches.iter().find(|b| b.id == "memory").unwrap();
    assert_eq!(mem.run_count, 0);
    assert!(mem.standing.is_empty());
}

#[test]
fn swe_standing_prefers_merged() {
    let d = fixture();
    let detail = adapters::bench_detail(root(&d), "swe").expect("swe detail");
    let agent = detail.standing.iter().find(|s| s.arm == "agent").unwrap();
    assert_eq!(agent.source, "merged");
    assert_eq!(agent.pass_rate, 1.0);
}

#[test]
fn swe_merged_float_latency_parses() {
    // Regression: a float `mean_latency_ms` must not drop the merged file.
    let d = fixture();
    let detail = adapters::bench_detail(root(&d), "swe").expect("swe detail");
    let merged = detail
        .runs
        .iter()
        .find(|r| r.is_merged)
        .expect("merged run present");
    assert_eq!(merged.run_id, "merged");
    assert_eq!(merged.mean_latency_ms, Some(182_741)); // 182740.5 rounded
}

#[test]
fn swe_run_count_excludes_merged_and_symlink() {
    let d = fixture();
    let detail = adapters::bench_detail(root(&d), "swe").expect("swe detail");
    // One individual + one merged; the `latest-*` symlink and the oracle/noop
    // diagnostic arms are skipped.
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.info.run_count, 1);
}

#[test]
fn swe_hides_diagnostic_arms() {
    let d = fixture();
    let detail = adapters::bench_detail(root(&d), "swe").expect("swe detail");
    let arms: Vec<&str> = detail.standing.iter().map(|s| s.arm.as_str()).collect();
    assert_eq!(arms, ["agent"], "only the agent arm is shown for SWE");
    assert!(
        detail
            .runs
            .iter()
            .all(|r| r.arm.as_deref() == Some("agent"))
    );
    // Search must not surface oracle/noop items either.
    let hits = adapters::search(root(&d), "repo__pkg-1");
    assert!(
        hits.iter()
            .filter(|h| h.bench == "swe")
            .all(|h| h.arm.as_deref() == Some("agent"))
    );
}

#[test]
fn swe_item_resolves_trace_and_patch_artifact() {
    let d = fixture();
    let run = adapters::run_detail(root(&d), "swe", "results-agent-2026-01-01__00-00-00")
        .expect("run detail");
    assert_eq!(run.items.len(), 1);
    let item = &run.items[0];
    assert!(item.passed);
    let trace = item.trace.as_ref().expect("trace resolved");
    assert_eq!(
        trace.trace,
        "trace/2026-01-01__00-00-00/agent/repo__pkg-1.trace.json"
    );
    assert!(trace.messages.is_some());
    match &item.extra {
        BenchExtra::Swe {
            repo, artifacts, ..
        } => {
            assert_eq!(repo, "repo/pkg");
            let patch = artifacts.iter().find(|a| a.path.contains('#')).unwrap();
            assert_eq!(
                patch.path,
                "runs/predictions-agent-2026-01-01__00-00-00.jsonl#repo__pkg-1"
            );
        }
        other => panic!("expected swe extra, got {other:?}"),
    }
}

#[test]
fn tb1_derives_trace_path_and_reads_metadata() {
    let d = fixture();
    let run = adapters::run_detail(
        root(&d),
        "terminal-bench-1.0",
        "results-2026-02-02__00-00-00",
    )
    .expect("run detail");
    assert_eq!(run.summary.model.as_deref(), Some("deepseek"));
    assert_eq!(run.summary.duration_ms, Some(300_000));
    let item = &run.items[0];
    let trace = item.trace.as_ref().expect("trace derived");
    assert_eq!(
        trace.trace,
        "trace/2026-02-02__00-00-00/mytask/mytask.1-of-1.2026-02-02__00-00-00/agent-logs/trace.json"
    );
    match &item.extra {
        BenchExtra::Tb { parser_results, .. } => {
            assert_eq!(parser_results.len(), 1);
            assert_eq!(parser_results[0].status, "passed");
        }
        other => panic!("expected tb extra, got {other:?}"),
    }
}

#[test]
fn tb2_uses_trace_path_field() {
    let d = fixture();
    let run = adapters::run_detail(
        root(&d),
        "terminal-bench-2.0",
        "results-2026-03-03__00-00-00",
    )
    .expect("run detail");
    let item = &run.items[0];
    assert!(!item.passed);
    let trace = item.trace.as_ref().expect("trace from field");
    assert_eq!(
        trace.trace,
        "trace/2026-03-03__00-00-00/t2/t2__abc/agent-logs/trace.json"
    );
    // The terminal-bench harness records no cost.
    assert_eq!(item.cost_micro_usd, None);
}

#[test]
fn search_matches_item_id_substring() {
    let d = fixture();
    let hits = adapters::search(root(&d), "repo__pkg");
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h.bench == "swe"));
    assert!(adapters::search(root(&d), "no-such-thing").is_empty());
}

// ── API router tests ─────────────────────────────────────────────────

mod api {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn get(root: &Path, uri: &str) -> (StatusCode, Vec<u8>) {
        let app = baybo_bench_web::api::router(root.to_path_buf());
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, bytes)
    }

    #[tokio::test]
    async fn benches_endpoint_ok() {
        let d = fixture();
        let (status, body) = get(root(&d), "/api/benches").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn unknown_bench_404() {
        let d = fixture();
        let (status, _) = get(root(&d), "/api/benches/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn path_traversal_rejected() {
        let d = fixture();
        let (status, _) = get(root(&d), "/api/benches/swe/file?path=../../../etc/passwd").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trace_endpoint_reshapes_envelope() {
        let d = fixture();
        let (status, body) = get(
            root(&d),
            "/api/benches/swe/trace?trace=trace/2026-01-01__00-00-00/agent/repo__pkg-1.trace.json&messages=trace/2026-01-01__00-00-00/agent/repo__pkg-1.messages.json",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["session_messages"].as_array().unwrap().len(), 1);
        assert_eq!(v["jobs"].as_array().unwrap().len(), 1);
    }
}
