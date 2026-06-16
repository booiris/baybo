//! Grade predictions with the **official** `swebench` harness — the canonical,
//! leaderboard-parity grader. We shell out to
//! `python -m swebench.harness.run_evaluation`, which builds/pulls each
//! instance's Docker image, applies the test patch + our prediction, runs the
//! `FAIL_TO_PASS` / `PASS_TO_PASS` tests, and writes a `<model>.<run_id>.json`
//! report. We parse that report back into [`GradeReport`].
//!
//! The `oracle` arm passes [`Predictions::Gold`] (`--predictions_path gold`),
//! so the harness supplies the gold patches itself — no aura, no prediction
//! file. That, plus the empty-patch `noop` arm, lets the whole Docker+grader
//! pipeline be validated offline (oracle ≈100%, noop 0%) before the agent arm
//! spends anything.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::process::Command;

/// What to grade: the harness's built-in gold patches, or our `predictions.jsonl`.
pub enum Predictions {
    /// `--predictions_path gold` — the oracle ceiling.
    Gold,
    /// A `predictions.jsonl` we wrote (agent or noop arm).
    File(PathBuf),
}

/// One harness invocation's inputs.
pub struct GraderConfig {
    pub python_bin: String,
    pub dataset_name: String,
    pub split: String,
    pub run_id: String,
    pub max_workers: usize,
    /// `model_name_or_path` — both the predictions label and the report-file
    /// stem (`<model_name>.<run_id>.json`). `"gold"` for the oracle arm.
    pub model_name: String,
    /// Directory the harness runs in (so its report + logs land here).
    pub runs_dir: PathBuf,
    /// Scope grading to exactly these instances (keeps image build cheap).
    pub instance_ids: Vec<String>,
    /// Image namespace: `"swebench"` pulls prebuilt Hub images (the harness
    /// default); `"none"` builds locally. Must match the instances' image keys.
    pub namespace: String,
}

/// The fields we read back from the harness's run report.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GradeReport {
    pub total_instances: usize,
    pub resolved_ids: Vec<String>,
    pub unresolved_ids: Vec<String>,
    pub error_ids: Vec<String>,
    pub empty_patch_ids: Vec<String>,
    pub completed_ids: Vec<String>,
}

impl GradeReport {
    /// Resolved-instance lookup set for joining against our instance list.
    pub fn resolved_set(&self) -> BTreeSet<&str> {
        self.resolved_ids.iter().map(String::as_str).collect()
    }
    pub fn empty_set(&self) -> BTreeSet<&str> {
        self.empty_patch_ids.iter().map(String::as_str).collect()
    }
    pub fn error_set(&self) -> BTreeSet<&str> {
        self.error_ids.iter().map(String::as_str).collect()
    }
}

/// Run the harness and parse its report. A non-zero exit is logged but not
/// fatal — the harness still writes a report when only some instances error;
/// only a missing report aborts.
pub async fn grade(cfg: &GraderConfig, predictions: &Predictions) -> Result<GradeReport> {
    std::fs::create_dir_all(&cfg.runs_dir)
        .with_context(|| format!("create runs dir {}", cfg.runs_dir.display()))?;

    let predictions_arg = match predictions {
        Predictions::Gold => "gold".to_string(),
        Predictions::File(path) => path
            .to_str()
            .context("predictions path is not utf-8")?
            .to_string(),
    };

    let mut args: Vec<String> = vec![
        "-m".into(),
        "swebench.harness.run_evaluation".into(),
        "--dataset_name".into(),
        cfg.dataset_name.clone(),
        "--split".into(),
        cfg.split.clone(),
        "--predictions_path".into(),
        predictions_arg,
        "--run_id".into(),
        cfg.run_id.clone(),
        "--max_workers".into(),
        cfg.max_workers.to_string(),
        // Match the instances' image keys: "swebench" => pull prebuilt Hub
        // images, "none" => local build. run_evaluation accepts "none" verbatim.
        "--namespace".into(),
        cfg.namespace.clone(),
    ];
    if !cfg.instance_ids.is_empty() {
        args.push("--instance_ids".into());
        args.extend(cfg.instance_ids.iter().cloned());
    }

    // The harness runs in `runs_dir` (so its report lands there). A relative
    // interpreter path (e.g. `bench/swe/.venv/bin/python`) would otherwise
    // resolve against that cwd and vanish — make it absolute first. Join the
    // process cwd rather than canonicalizing: a venv's `python` is a symlink to
    // the base interpreter, and following it would lose the venv's site-packages
    // (swebench). A bare command (`python`) is a `$PATH` lookup, left untouched.
    let python_path = std::path::Path::new(&cfg.python_bin);
    let python = if cfg.python_bin.contains('/') && python_path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(python_path).to_string_lossy().into_owned())
            .unwrap_or_else(|_| cfg.python_bin.clone())
    } else {
        cfg.python_bin.clone()
    };
    tracing::info!(
        python = %python,
        run_id = %cfg.run_id,
        instances = cfg.instance_ids.len(),
        "running swebench harness (this builds Docker images and runs tests)"
    );
    let status = Command::new(&python)
        .args(&args)
        .current_dir(&cfg.runs_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| {
            format!(
                "spawn `{} -m swebench.harness.run_evaluation` — is the swebench \
                 package installed (pip install swebench) and Docker running?",
                cfg.python_bin
            )
        })?;
    if !status.success() {
        tracing::warn!(%status, "swebench harness exited non-zero; reading any report it wrote");
    }

    let report_path = locate_report(&cfg.runs_dir, &cfg.model_name, &cfg.run_id)?;
    let raw = std::fs::read_to_string(&report_path)
        .with_context(|| format!("read harness report {}", report_path.display()))?;
    parse_report(&raw)
}

/// The harness writes `<model_name>.<run_id>.json` to its working directory.
/// Look there first, then fall back to the process CWD, so a future harness
/// tweak that ignores `current_dir` still resolves.
fn locate_report(runs_dir: &Path, model_name: &str, run_id: &str) -> Result<PathBuf> {
    let file = format!("{model_name}.{run_id}.json");
    let candidates = [runs_dir.join(&file), PathBuf::from(&file)];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    bail!(
        "harness report `{file}` not found (looked in {} and the current dir) — \
         the harness may have failed before writing it; check its output above",
        runs_dir.display()
    )
}

/// Parse the harness report JSON. We read only the `*_ids` arrays + the total;
/// every field defaults to empty so a harness-version field rename degrades to
/// "nothing resolved" rather than a parse failure.
pub fn parse_report(raw: &str) -> Result<GradeReport> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("parse swebench report as JSON")?;
    let ids = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(GradeReport {
        total_instances: value
            .get("total_instances")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        resolved_ids: ids("resolved_ids"),
        unresolved_ids: ids("unresolved_ids"),
        error_ids: ids("error_ids"),
        empty_patch_ids: ids("empty_patch_ids"),
        completed_ids: ids("completed_ids"),
    })
}

/// Summarize WHY a graded instance didn't resolve, by reading the harness's
/// **per-instance** artifacts under
/// `<runs_dir>/logs/run_evaluation/<run_id>/<model>/<id>/`: `report.json`
/// (which `FAIL_TO_PASS`/`PASS_TO_PASS` tests are still red) plus the sibling
/// `test_output.txt` (the actual exception/assertion lines from the test run).
/// The run-level report parsed by [`parse_report`] only buckets ids. Returns
/// `None` when the instance resolved, the report is absent/unparseable, or
/// nothing stands out.
pub fn failure_reason(
    runs_dir: &Path,
    run_id: &str,
    model_name: &str,
    instance_id: &str,
) -> Option<String> {
    let dir = runs_dir
        .join("logs/run_evaluation")
        .join(run_id)
        .join(model_name)
        .join(instance_id);
    let report = std::fs::read_to_string(dir.join("report.json")).ok()?;
    let test_output = std::fs::read_to_string(dir.join("test_output.txt")).ok();
    reason_from_report(&report, test_output.as_deref(), instance_id)
}

const MAX_LISTED_TESTS: usize = 3;

/// Categorize an unresolved instance from its per-instance report JSON, then
/// enrich it with the concrete exception lines from the harness
/// `test_output.txt` (when supplied). Pure (no I/O) so it's unit-testable.
/// `None` if the record says resolved or carries no actionable failure.
fn reason_from_report(raw: &str, test_output: Option<&str>, instance_id: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let rec = value.get(instance_id)?;
    if rec.get("resolved").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    // A genuine apply failure shows as patch_successfully_applied=false. (A
    // reverse-applied patch is mislabeled true by swebench — that case surfaces
    // below as the FAIL_TO_PASS tests then failing.)
    if rec
        .get("patch_successfully_applied")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        return Some("patch failed to apply".to_string());
    }
    let ts = rec.get("tests_status")?;
    let failures = |group: &str| -> Vec<String> {
        ts.get(group)
            .and_then(|g| g.get("failure"))
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let f2p = failures("FAIL_TO_PASS");
    let p2p = failures("PASS_TO_PASS");
    let head = if !f2p.is_empty() {
        format!("FAIL_TO_PASS still failing: {}", summarize_tests(&f2p))
    } else if !p2p.is_empty() {
        format!("PASS_TO_PASS regressed: {}", summarize_tests(&p2p))
    } else {
        return None;
    };
    let errors = test_output.map(extract_error_lines).unwrap_or_default();
    if errors.is_empty() {
        return Some(head);
    }
    let mut s = head;
    for e in errors {
        s.push_str("\n↳ ");
        s.push_str(&e);
    }
    Some(s)
}

/// Join up to [`MAX_LISTED_TESTS`] test ids, appending `(+N more)` if truncated.
fn summarize_tests(tests: &[String]) -> String {
    let shown = tests.len().min(MAX_LISTED_TESTS);
    let mut s = tests[..shown].join(", ");
    if tests.len() > shown {
        s.push_str(&format!(" (+{} more)", tests.len() - shown));
    }
    s
}

const MAX_ERROR_LINES: usize = 5;
const MAX_ERROR_LINE_LEN: usize = 200;
const EXCEPTION_SUFFIXES: [&str; 3] = ["Error", "Exception", "Failed"];

/// Pull the concrete exception/assertion lines out of a harness
/// `test_output.txt`, tolerant of the runner formats SWE-bench uses: pytest
/// (`E   <Exc>: msg`, astropy/matplotlib), and unittest/django + sympy
/// (`<Exc>: msg` at column 0). ANSI color codes (astropy) are stripped first.
/// Prefers typed-exception lines; falls back to raw pytest `E` lines (e.g. a
/// bare `assert` expansion) when no exception type is present.
fn extract_error_lines(test_output: &str) -> Vec<String> {
    let clean = strip_ansi(test_output);
    let mut primary: Vec<String> = Vec::new();
    let mut fallback: Vec<String> = Vec::new();
    for raw in clean.lines() {
        let line = raw.trim_end();
        if let Some(content) = pytest_e_marker(line) {
            if looks_like_exception(content) {
                if push_capped(&mut primary, content) {
                    break;
                }
            } else if fallback.len() < MAX_ERROR_LINES {
                push_capped(&mut fallback, content);
            }
        } else if !line.starts_with(char::is_whitespace)
            && looks_like_exception(line)
            && push_capped(&mut primary, line)
        {
            break;
        }
    }
    if primary.is_empty() {
        fallback
    } else {
        primary
    }
}

/// Pytest tags each line of a failure's captured exception with a leading `E`
/// + whitespace; return the content after that marker.
fn pytest_e_marker(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('E')?;
    if rest.starts_with(char::is_whitespace) {
        let t = rest.trim_start();
        return (!t.is_empty()).then_some(t);
    }
    None
}

/// Whether `text` opens with a Python-exception-shaped token (`AssertionError`,
/// `ValueError: …`, `module.Custom Error`, …). Used to keep prose / source
/// lines out of the extracted reason.
fn looks_like_exception(text: &str) -> bool {
    let token = text
        .split(|c: char| c == ':' || c == '(' || c.is_whitespace())
        .next()
        .unwrap_or("");
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && EXCEPTION_SUFFIXES.iter().any(|s| token.ends_with(s))
}

/// Append `text` (trimmed, length-capped, deduped) to `out`. Returns whether
/// `out` has reached [`MAX_ERROR_LINES`].
fn push_capped(out: &mut Vec<String>, text: &str) -> bool {
    let t = truncate_chars(text.trim(), MAX_ERROR_LINE_LEN);
    if !t.is_empty() && !out.iter().any(|e| e == &t) {
        out.push(t);
    }
    out.len() >= MAX_ERROR_LINES
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Strip ANSI CSI escape sequences (`ESC [ … <final-byte>`) — astropy's pytest
/// run colorizes its output, which would otherwise pollute the extracted lines.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // ESC: consume a CSI sequence (`[` … terminator in 0x40..=0x7E). A lone
        // ESC with no `[` is simply dropped.
        let mut rest = chars.clone();
        if rest.next() == Some('[') {
            for n in rest.by_ref() {
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
            chars = rest;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors `swebench`'s `make_run_report` output shape.
    const SAMPLE: &str = r#"{
      "total_instances": 3,
      "submitted_instances": 3,
      "completed_instances": 2,
      "resolved_instances": 1,
      "unresolved_instances": 1,
      "empty_patch_instances": 1,
      "error_instances": 0,
      "completed_ids": ["django__django-1", "django__django-2"],
      "resolved_ids": ["django__django-1"],
      "unresolved_ids": ["django__django-2"],
      "empty_patch_ids": ["sympy__sympy-9"],
      "error_ids": [],
      "schema_version": 2
    }"#;

    #[test]
    fn parses_resolved_and_totals() {
        let r = parse_report(SAMPLE).unwrap();
        assert_eq!(r.total_instances, 3);
        assert_eq!(r.resolved_ids, vec!["django__django-1"]);
        assert_eq!(r.unresolved_ids, vec!["django__django-2"]);
        assert_eq!(r.empty_patch_ids, vec!["sympy__sympy-9"]);
        assert!(r.error_ids.is_empty());
        assert!(r.resolved_set().contains("django__django-1"));
        assert!(!r.resolved_set().contains("django__django-2"));
    }

    #[test]
    fn missing_fields_default_to_empty() {
        let r = parse_report(r#"{"total_instances": 0}"#).unwrap();
        assert_eq!(r.total_instances, 0);
        assert!(r.resolved_ids.is_empty());
        assert!(r.completed_ids.is_empty());
    }

    #[test]
    fn rejects_non_json() {
        assert!(parse_report("not json").is_err());
    }

    const REPORT: &str = r#"{"sphinx-doc__sphinx-1":{"patch_successfully_applied":true,"resolved":false,
        "tests_status":{"FAIL_TO_PASS":{"success":[],"failure":["t/x.py::a","t/x.py::b","t/x.py::c","t/x.py::d"]},
        "PASS_TO_PASS":{"success":["t/y.py::ok"],"failure":[]}}}}"#;

    #[test]
    fn reason_reports_fail_to_pass_with_truncation() {
        let r = reason_from_report(REPORT, None, "sphinx-doc__sphinx-1").unwrap();
        assert!(r.starts_with("FAIL_TO_PASS still failing:"));
        assert!(r.contains("t/x.py::a"));
        assert!(r.contains("(+1 more)")); // 4 failures, 3 shown
    }

    #[test]
    fn reason_reports_pass_to_pass_regression() {
        let raw = r#"{"i":{"patch_successfully_applied":true,"resolved":false,
            "tests_status":{"FAIL_TO_PASS":{"success":["t::a"],"failure":[]},
            "PASS_TO_PASS":{"success":[],"failure":["t::z"]}}}}"#;
        let r = reason_from_report(raw, None, "i").unwrap();
        assert_eq!(r, "PASS_TO_PASS regressed: t::z");
    }

    #[test]
    fn reason_reports_apply_failure() {
        let raw =
            r#"{"i":{"patch_successfully_applied":false,"resolved":false,"tests_status":{}}}"#;
        assert_eq!(
            reason_from_report(raw, None, "i").unwrap(),
            "patch failed to apply"
        );
    }

    #[test]
    fn reason_none_when_resolved_or_missing() {
        let raw = r#"{"i":{"patch_successfully_applied":true,"resolved":true,"tests_status":{}}}"#;
        assert!(reason_from_report(raw, None, "i").is_none());
        // Record for a different id => None (not this instance's report).
        assert!(reason_from_report(REPORT, None, "other").is_none());
        assert!(reason_from_report("not json", None, "i").is_none());
    }

    #[test]
    fn reason_appends_extracted_exception_lines() {
        // django-style unittest output: `<Exc>: msg` at column 0.
        let out = "FAIL: test_choices (m.C)\n\
                   ----\nTraceback (most recent call last):\n\
                   \x20 File \"x.py\", line 1, in test_choices\n\
                   AssertionError: Lists differ: ['a'] != ['b']\n";
        let r = reason_from_report(REPORT, Some(out), "sphinx-doc__sphinx-1").unwrap();
        assert!(r.starts_with("FAIL_TO_PASS still failing:"));
        assert!(r.contains("↳ AssertionError: Lists differ: ['a'] != ['b']"));
    }

    #[test]
    fn extract_handles_pytest_e_marker_and_ansi() {
        // pytest with astropy-style ANSI color codes around the E line.
        let out = "    def test_x():\n>       foo()\n\
                   \x1b[1m\x1b[31mE       AttributeError: module 'm' has no attribute 'foo'\x1b[0m\n\
                   lib/m/__init__.py:153: AttributeError\n";
        let lines = extract_error_lines(out);
        assert_eq!(
            lines,
            vec!["AttributeError: module 'm' has no attribute 'foo'".to_string()]
        );
    }

    #[test]
    fn extract_falls_back_to_assert_expansion_when_untyped() {
        // A bare `assert` failure: pytest prints `E   assert …` with no typed
        // exception on an E line; the footer `file:line: AssertionError` is not
        // column 0, so the fallback `assert` line is what we surface.
        let out = "    def test_x():\n>       assert add(1, 2) == 4\n\
                   E       assert 3 == 4\n\
                   test_x.py:2: AssertionError\n";
        let lines = extract_error_lines(out);
        assert_eq!(lines, vec!["assert 3 == 4".to_string()]);
    }

    #[test]
    fn extract_ignores_uppercase_pip_error_noise() {
        // pip's `ERROR:` (all caps) must not be mistaken for an exception line.
        let out = "ERROR: pip's dependency resolver does not currently take...\n\
                   AssertionError: real failure\n";
        assert_eq!(
            extract_error_lines(out),
            vec!["AssertionError: real failure".to_string()]
        );
    }

    #[test]
    fn extract_dedupes_and_caps() {
        let mut out = String::new();
        for _ in 0..3 {
            out.push_str("AssertionError: same message\n");
        }
        for i in 0..10 {
            out.push_str(&format!("ValueError: distinct {i}\n"));
        }
        let lines = extract_error_lines(&out);
        assert_eq!(lines.len(), MAX_ERROR_LINES);
        assert_eq!(lines[0], "AssertionError: same message");
    }
}
