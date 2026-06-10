//! Shared data layer for the SWE-bench benchmark: the normalized instance IR,
//! the prompt the agent is handed, the `predictions.jsonl` line shape the
//! `swebench` harness consumes, and the per-arm model label. Holds **no Aura,
//! Docker, or Python dependency** so it stays fast to compile and unit-testable.
//!
//! The flow the bins implement around these types:
//! 1. `swe_export.py` writes `instances.json` (dataset rows + the canonical
//!    Docker `image_key`), which [`load_instances`] parses into [`SweInstance`]s.
//! 2. The `agent` arm runs aura inside each instance's image and captures a
//!    `git diff`; that becomes a [`prediction_line`] in `predictions.jsonl`.
//! 3. The `swebench` harness grades the predictions (or `gold`); the report is
//!    parsed back in `grader.rs`.

use serde::{Deserialize, Serialize};

pub mod agent;
pub mod grader;
pub mod report;

/// One normalized SWE-bench task instance, as exported by `swe_export.py`.
///
/// `image_key` is the Docker image the **official** harness uses for this
/// instance (from `swebench`'s `make_test_spec(...).instance_image_key`), so the
/// agent arm runs aura inside the *exact* environment grading will use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweInstance {
    pub instance_id: String,
    pub repo: String,
    pub base_commit: String,
    pub problem_statement: String,
    #[serde(default)]
    pub version: String,
    pub image_key: String,
}

/// Read the `instances.json` array produced by `swe_export.py`.
pub fn load_instances(path: &std::path::Path) -> anyhow::Result<Vec<SweInstance>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read instances file {}: {e}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse instances file {}: {e}", path.display()))
}

/// The `model_name_or_path` written into predictions and used to locate the
/// harness report file (`<model>.<run_id>.json`). The `gold`/oracle arm reuses
/// the harness's own `"gold"` label; our two prediction-producing arms get
/// distinct labels so their reports never overwrite each other.
pub fn arm_model_name(arm: &str) -> &'static str {
    match arm {
        "oracle" => "gold",
        "noop" => "aura-noop",
        _ => "aura",
    }
}

/// The instruction handed to `aura prompt` for one instance. The repo is already
/// checked out at `base_commit` in the image at `/testbed` (the agent's cwd), so
/// the agent just edits in place; we capture the diff afterwards. Tests are
/// withheld (the harness applies them at grade time), so the agent is told not
/// to touch them — edits there are reset by the grader anyway.
pub fn frame_instruction(instance: &SweInstance) -> String {
    let SweInstance {
        repo,
        problem_statement,
        ..
    } = instance;
    format!(
        r#"You are working in a checked-out clone of the `{repo}` repository, located at `/testbed` (your current working directory). Resolve the following GitHub issue by editing the project's source code in place.

Guidelines:
- Make the minimal change that fixes the issue described below.
- Edit ONLY the library's source code. Do NOT modify, add, or delete test files, or build/packaging/config files (setup.py, pyproject.toml, setup.cfg, requirements*.txt, tox.ini, CI configs) — the evaluation supplies its own tests and environment, and changing build config can break it.
- You may run the project's tools (it is fully installed in this environment) to explore and verify your fix.
- When you are confident the issue is resolved, stop. Your changes on disk are graded automatically; you do not need to commit.

--- ISSUE ---
{problem_statement}"#
    )
}

/// One line of `predictions.jsonl` for the `swebench` harness.
pub fn prediction_line(instance_id: &str, model_name: &str, patch: &str) -> serde_json::Value {
    serde_json::json!({
        "instance_id": instance_id,
        "model_name_or_path": model_name,
        "model_patch": patch,
    })
}

/// Serialize predictions to the JSONL text the harness reads (one object/line).
pub fn predictions_jsonl(lines: &[serde_json::Value]) -> String {
    lines
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Default run id: a sortable timestamp plus a short random suffix, so reruns
/// never collide on the harness's `<model>.<run_id>.json` report file.
pub fn default_run_id() -> String {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{ts}-{}", &suffix[..6])
}

/// Split an agent-model spec `<provider>/<model>` into `(provider, model)`.
/// A spec with no `/` (or an empty half) assumes the `deepseek` provider —
/// matching the terminal bench's `--model <provider>/<model>` convention.
pub fn parse_model(spec: &str) -> (String, String) {
    match spec.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (provider.to_string(), model.to_string())
        }
        _ => ("deepseek".to_string(), spec.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_splits_provider_and_model() {
        assert_eq!(
            parse_model("openai/gpt-4o"),
            ("openai".to_string(), "gpt-4o".to_string())
        );
        assert_eq!(
            parse_model("anthropic/claude-3-5-sonnet"),
            ("anthropic".to_string(), "claude-3-5-sonnet".to_string())
        );
        // No `provider/` prefix => deepseek (terminal-bench convention).
        assert_eq!(
            parse_model("deepseek-v4-flash"),
            ("deepseek".to_string(), "deepseek-v4-flash".to_string())
        );
    }

    fn instance() -> SweInstance {
        SweInstance {
            instance_id: "django__django-12345".to_string(),
            repo: "django/django".to_string(),
            base_commit: "abc123".to_string(),
            problem_statement: "Fix the widget rendering bug.".to_string(),
            version: "4.1".to_string(),
            image_key: "swebench/sweb.eval.x86_64.django__django-12345:latest".to_string(),
        }
    }

    #[test]
    fn arm_model_names_are_distinct_and_gold_for_oracle() {
        assert_eq!(arm_model_name("oracle"), "gold");
        assert_eq!(arm_model_name("noop"), "aura-noop");
        assert_eq!(arm_model_name("agent"), "aura");
        // Distinct so concurrent arms' report files never clobber each other.
        assert_ne!(arm_model_name("noop"), arm_model_name("agent"));
    }

    #[test]
    fn frame_instruction_carries_repo_issue_and_testbed() {
        let framed = frame_instruction(&instance());
        assert!(framed.contains("/testbed"));
        assert!(framed.contains("django/django"));
        assert!(framed.contains("Fix the widget rendering bug."));
        // Must steer the agent away from editing tests.
        assert!(framed.to_lowercase().contains("test"));
    }

    #[test]
    fn prediction_line_has_the_three_harness_keys() {
        let line = prediction_line("django__django-12345", "aura", "diff --git a/x b/x");
        assert_eq!(line["instance_id"], "django__django-12345");
        assert_eq!(line["model_name_or_path"], "aura");
        assert_eq!(line["model_patch"], "diff --git a/x b/x");
    }

    #[test]
    fn predictions_jsonl_is_one_object_per_line() {
        let lines = vec![
            prediction_line("a", "aura", "pa"),
            prediction_line("b", "aura", "pb"),
        ];
        let text = predictions_jsonl(&lines);
        let parsed: Vec<&str> = text.lines().collect();
        assert_eq!(parsed.len(), 2);
        for l in parsed {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v.get("instance_id").is_some());
        }
    }

    #[test]
    fn instances_round_trip_through_json() {
        let json = serde_json::to_string(&vec![instance()]).unwrap();
        let back: Vec<SweInstance> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].instance_id, "django__django-12345");
        assert_eq!(
            back[0].image_key,
            "swebench/sweb.eval.x86_64.django__django-12345:latest"
        );
    }

    #[test]
    fn instances_tolerate_missing_version() {
        let back: Vec<SweInstance> = serde_json::from_str(
            r#"[{"instance_id":"a","repo":"r","base_commit":"c","problem_statement":"p","image_key":"k"}]"#,
        )
        .unwrap();
        assert_eq!(back[0].version, "");
    }
}
