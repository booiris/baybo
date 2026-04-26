#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aura_sandbox::sandbox_exec::SandboxExecRunner;
use aura_sandbox::{EnvPolicy, NetworkPolicy, SandboxRunner, SandboxSpec, StdinSource};

fn sandbox_exec_present() -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|d| d.join("sandbox-exec").is_file())
}

#[tokio::test]
async fn echo_through_sandbox_exec() {
    if !sandbox_exec_present() {
        eprintln!("skipping: sandbox-exec not on $PATH");
        return;
    }
    let runner: Arc<dyn SandboxRunner> =
        Arc::new(SandboxExecRunner::discover().expect("sandbox-exec runner"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = runner
        .run(SandboxSpec {
            program: PathBuf::from("/bin/echo"),
            args: vec!["hello".into()],
            cwd: None,
            workspace_root: tmp.path().to_path_buf(),
            readable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(5),
        })
        .await
        .expect("sandboxed run");
    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
}

/// Regression for the v1 macOS write profile: the SBPL policy should
/// permit writes only inside the workspace bind. A sandboxed Bash that
/// tries to drop a marker into `/tmp` (which resolves to `/private/tmp`
/// on macOS) must fail and must not leave the file behind on the host.
#[tokio::test]
async fn host_tmp_writes_are_denied() {
    if !sandbox_exec_present() {
        eprintln!("skipping: sandbox-exec not on $PATH");
        return;
    }
    let runner: Arc<dyn SandboxRunner> =
        Arc::new(SandboxExecRunner::discover().expect("sandbox-exec runner"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = format!("/tmp/aura-sandbox-host-escape-{}", std::process::id());
    let _ = std::fs::remove_file(&marker);
    let out = runner
        .run(SandboxSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), format!("touch {marker} && echo ESCAPED")],
            cwd: None,
            workspace_root: tmp.path().to_path_buf(),
            readable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(5),
        })
        .await
        .expect("sandboxed run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("ESCAPED"),
        "touch must fail and short-circuit the && chain (stdout was {stdout})"
    );
    assert!(
        !std::path::Path::new(&marker).exists(),
        "marker `{marker}` must not exist on host after sandboxed run"
    );
}
