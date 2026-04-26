use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aura_sandbox::{
    EnvPolicy, ResourceLimits, SandboxOutput, SandboxRunner, SandboxSpec, StdinSource,
};
use tokio_util::sync::CancellationToken;

use crate::error::CodeBuilderError;
use crate::plan::EffectivePlan;
use crate::scratch::RunDir;

pub(crate) fn build_sandbox_spec(
    plan: &EffectivePlan,
    scratch: &RunDir,
    uv_path: &Path,
) -> SandboxSpec {
    let mut args: Vec<String> = Vec::with_capacity(16);
    args.push(format!("UV_CACHE_DIR={}", scratch.uv_cache_dir.display()));
    args.push(format!(
        "UV_PROJECT_ENVIRONMENT={}/.venv",
        scratch.root.display()
    ));
    args.push(format!("HOME={}", scratch.root.display()));
    args.push("PYTHONUNBUFFERED=1".into());
    args.push("UV_NO_PROGRESS=1".into());

    args.push(uv_path.to_string_lossy().into_owned());
    args.push("run".into());
    args.push("--isolated".into());
    args.push("--no-project".into());
    args.push("--quiet".into());
    args.push("--script".into());
    args.push(scratch.script_path.to_string_lossy().into_owned());

    let _ = plan; // dependencies travel inside the script via PEP 723 inline metadata.

    SandboxSpec {
        program: PathBuf::from("/usr/bin/env"),
        args,
        cwd: Some(scratch.workdir.clone()),
        workspace_root: scratch.root.clone(),
        readable_paths: plan.readable_paths.clone(),
        allowed_hosts: BTreeSet::new(),
        network_policy: plan.network_policy,
        env: EnvPolicy::Allowlist {
            vars: vec!["PATH".into(), "LANG".into(), "LC_ALL".into(), "TZ".into()],
        },
        stdin: StdinSource::Null,
        timeout: Duration::from_secs(plan.wall_clock_seconds),
        resource_limits: ResourceLimits {
            memory_max_bytes: Some(plan.memory_max_bytes),
            pids_max: Some(plan.pids_max),
        },
    }
}

pub(crate) async fn execute(
    runner: Arc<dyn SandboxRunner>,
    spec: SandboxSpec,
    cancel: CancellationToken,
) -> Result<SandboxOutput, CodeBuilderError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(CodeBuilderError::Cancelled),
        res = runner.run(spec) => res.map_err(CodeBuilderError::from),
    }
}

pub(crate) fn truncate_utf8(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut cut = max;
    while cut > 0 && (bytes[cut] & 0b1100_0000) == 0b1000_0000 {
        cut -= 1;
    }
    let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    s.push_str("\n… [truncated]");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_sandbox::NetworkPolicy;

    fn dummy_plan() -> EffectivePlan {
        EffectivePlan {
            code: "print(1)".into(),
            network_policy: NetworkPolicy::None,
            readable_paths: vec![PathBuf::from("/data")],
            wall_clock_seconds: 30,
            memory_max_bytes: 256 * 1024 * 1024,
            pids_max: 64,
            rationale: "test".into(),
        }
    }

    #[test]
    fn spec_carries_uv_argv_and_per_call_env() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = RunDir::create(tmp.path()).unwrap();
        let uv = PathBuf::from("/usr/local/bin/uv");

        let spec = build_sandbox_spec(&dummy_plan(), &scratch, &uv);
        assert_eq!(spec.program, PathBuf::from("/usr/bin/env"));
        assert!(spec.args.iter().any(|a| a.starts_with("UV_CACHE_DIR=")));
        assert!(spec.args.iter().any(|a| a == "--isolated"));
        assert!(spec.args.iter().any(|a| a == "--no-project"));
        assert!(spec.args.iter().any(|a| a == "--script"));
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a.starts_with("CODE_BUILDER_INPUTS_PATH=")),
            "inputs env var must be gone"
        );
        assert_eq!(spec.timeout, Duration::from_secs(30));
        assert_eq!(
            spec.resource_limits.memory_max_bytes,
            Some(256 * 1024 * 1024)
        );
        assert_eq!(spec.resource_limits.pids_max, Some(64));
        assert_eq!(spec.workspace_root, scratch.root);
        assert_eq!(spec.cwd.as_deref(), Some(scratch.workdir.as_path()));
    }

    #[test]
    fn no_input_values_appear_on_argv() {
        // Regression for adversarial review #3: input values must never
        // be visible in process argv. The path on argv is fine; values
        // live inside a 0600 file under the scratch dir.
        let tmp = tempfile::tempdir().unwrap();
        let scratch = RunDir::create(tmp.path()).unwrap();
        let uv = PathBuf::from("/usr/local/bin/uv");

        let spec = build_sandbox_spec(&dummy_plan(), &scratch, &uv);
        for a in &spec.args {
            assert!(
                !a.starts_with("CODE_BUILDER_INPUT_"),
                "found per-input argv entry: {a:?}"
            );
        }
    }

    #[test]
    fn truncate_utf8_respects_char_boundary() {
        let bytes = "héllo".as_bytes();
        assert_eq!(truncate_utf8(bytes, 100), "héllo");
        let cut = truncate_utf8(bytes, 2);
        assert!(cut.starts_with('h'));
        assert!(cut.contains("[truncated]"));
    }
}
