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
    pub results_dir: PathBuf,
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
    std::fs::create_dir_all(&cfg.results_dir)
        .with_context(|| format!("create results dir {}", cfg.results_dir.display()))?;

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

    // The harness runs in `results_dir` (so its report lands there). A relative
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
        .current_dir(&cfg.results_dir)
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

    let report_path = locate_report(&cfg.results_dir, &cfg.model_name, &cfg.run_id)?;
    let raw = std::fs::read_to_string(&report_path)
        .with_context(|| format!("read harness report {}", report_path.display()))?;
    parse_report(&raw)
}

/// The harness writes `<model_name>.<run_id>.json` to its working directory.
/// Look there first, then fall back to the process CWD, so a future harness
/// tweak that ignores `current_dir` still resolves.
fn locate_report(results_dir: &Path, model_name: &str, run_id: &str) -> Result<PathBuf> {
    let file = format!("{model_name}.{run_id}.json");
    let candidates = [results_dir.join(&file), PathBuf::from(&file)];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    bail!(
        "harness report `{file}` not found (looked in {} and the current dir) — \
         the harness may have failed before writing it; check its output above",
        results_dir.display()
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
}
