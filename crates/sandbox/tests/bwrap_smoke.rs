#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use baybo_sandbox::bwrap::BwrapRunner;
use baybo_sandbox::{
    EnvPolicy, NetworkPolicy, ResourceLimits, SandboxRunner, SandboxSpec, StdinSource,
};

fn bwrap_present() -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|d| d.join("bwrap").is_file())
}

#[tokio::test]
async fn echo_through_bwrap() {
    if !bwrap_present() {
        eprintln!("skipping: bwrap not installed on $PATH");
        return;
    }
    // Construct the bwrap runner directly so the docker fallback in
    // `current_platform_runner()` can't shadow what this test exercises.
    let runner: Arc<dyn SandboxRunner> = Arc::new(BwrapRunner::discover().expect("bwrap runner"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = runner
        .run(SandboxSpec {
            program: PathBuf::from("/bin/echo"),
            args: vec!["hello".into()],
            cwd: None,
            workspace_root: tmp.path().to_path_buf(),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(5),
            resource_limits: ResourceLimits::unlimited(),
            filesystem_policy: baybo_sandbox::FilesystemPolicy::default(),
        })
        .await
        .expect("sandboxed run");
    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
}
