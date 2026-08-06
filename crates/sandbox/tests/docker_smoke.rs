#![cfg(feature = "docker")]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use baybo_sandbox::docker::{DEFAULT_IMAGE, DockerRunner};
use baybo_sandbox::{
    EnvPolicy, NetworkPolicy, ResourceLimits, SandboxRunner, SandboxSpec, StdinSource,
};

/// Returns whether docker is usable for these tests: the CLI is on PATH,
/// the daemon answers, and [`DEFAULT_IMAGE`] is already pulled. CLI-on-PATH
/// alone is not enough — most CI / dev hosts have the binary but not the
/// daemon socket, and stock CI runners often have a live daemon yet have
/// never pulled the base image (the tests don't `warm()`, so a missing
/// image is a hard failure rather than a pull). We bail out cleanly in
/// those cases so the suite still passes on those machines.
fn docker_reachable() -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    if !std::env::split_paths(&path_var).any(|d| d.join("docker").is_file()) {
        return false;
    }
    let ok = |args: &[&str]| {
        std::process::Command::new("docker")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    ok(&["info"]) && ok(&["image", "inspect", DEFAULT_IMAGE])
}

#[tokio::test]
async fn echo_through_docker() {
    if !docker_reachable() {
        eprintln!("skipping: docker daemon not reachable");
        return;
    }
    let runner: Arc<dyn SandboxRunner> = Arc::new(
        DockerRunner::discover(baybo_process::ProcessManager::transient()).expect("docker runner"),
    );
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
            timeout: Duration::from_secs(120),
            resource_limits: ResourceLimits::default(),
            filesystem_policy: baybo_sandbox::FilesystemPolicy::default(),
        })
        .await
        .expect("sandboxed run");
    assert_eq!(
        out.exit_code,
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
}

/// Regression for codex Issue #1 (high): a timed-out docker run must
/// not leave the daemon-side container alive. Without explicit
/// `docker rm -f`, the workload keeps running with the workspace
/// bind still mounted and continues to write past the timeout
/// boundary the sandbox returned.
#[tokio::test]
async fn timeout_force_removes_container() {
    if !docker_reachable() {
        eprintln!("skipping: docker daemon not reachable");
        return;
    }
    let runner =
        DockerRunner::discover(baybo_process::ProcessManager::transient()).expect("docker runner");
    runner.warm().await.expect("warm-up (image pull) succeeds");
    let runner: Arc<dyn SandboxRunner> = Arc::new(runner);
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("post-timeout-marker");
    let marker_in_container = marker.to_string_lossy().to_string();

    // Sleep long enough that the timeout below fires before the
    // workload's `touch` ever runs. If cleanup fails, the marker
    // appears on host disk after we observe `Timeout` — that's the
    // exact bug the cleanup code is supposed to prevent.
    let res = runner
        .run(SandboxSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                format!("sleep 4 && touch {marker_in_container}"),
            ],
            cwd: None,
            workspace_root: tmp.path().to_path_buf(),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_millis(800),
            resource_limits: ResourceLimits::default(),
            filesystem_policy: baybo_sandbox::FilesystemPolicy::default(),
        })
        .await;
    assert!(
        matches!(res, Err(baybo_sandbox::SandboxError::Timeout(_))),
        "expected Timeout, got: {res:?}",
    );

    // Wait past when the workload's sleep would have completed.
    // If the daemon-side container was not killed, the touch fires
    // and the marker file appears on host disk.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !marker.exists(),
        "container survived timeout: marker `{marker_in_container}` was written after the timeout returned",
    );
}

/// Regression: when the caller drops `runner.run()` (e.g. an outer
/// `tokio::time::timeout` or a `select!` cancellation upstream),
/// `kill_on_drop` only kills the local docker CLI — the daemon-side
/// container survives unless the runner's RAII cleanup guard fires
/// on future-drop. Without that guard the workload keeps running
/// and continues to write into the workspace bind after the caller
/// has already returned an error.
#[tokio::test]
async fn external_future_drop_force_removes_container() {
    if !docker_reachable() {
        eprintln!("skipping: docker daemon not reachable");
        return;
    }
    let runner =
        DockerRunner::discover(baybo_process::ProcessManager::transient()).expect("docker runner");
    runner.warm().await.expect("warm-up (image pull) succeeds");
    let runner: Arc<dyn SandboxRunner> = Arc::new(runner);
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("post-cancel-marker");
    let marker_in_container = marker.to_string_lossy().to_string();

    let spec = SandboxSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            format!("sleep 4 && touch {marker_in_container}"),
        ],
        cwd: None,
        workspace_root: tmp.path().to_path_buf(),
        readable_paths: vec![],
        writable_paths: vec![],
        allowed_hosts: BTreeSet::new(),
        network_policy: NetworkPolicy::None,
        env: EnvPolicy::Baseline,
        stdin: StdinSource::Null,
        // Inner sandbox timeout is intentionally far longer than the
        // outer wrapper timeout below — we want the future to be
        // dropped by the outer timeout before the runner's own
        // timeout branch ever runs.
        timeout: Duration::from_secs(60),
        resource_limits: ResourceLimits::default(),
        filesystem_policy: baybo_sandbox::FilesystemPolicy::default(),
    };

    let res = tokio::time::timeout(Duration::from_millis(800), runner.run(spec)).await;
    assert!(
        res.is_err(),
        "expected the outer wrapper timeout to fire first, got: {res:?}"
    );

    // Wait past the workload's own `sleep 4`. If the cleanup guard
    // missed the future-drop case, the touch fires inside the
    // surviving container and the marker shows up on host disk.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !marker.exists(),
        "container survived future drop: marker `{marker_in_container}` was written after the outer timeout returned",
    );
}

/// The Docker runner pins `--network none` for `NetworkPolicy::None`.
/// Confirm that an attempt to resolve a hostname fails inside the
/// container even on hosts with full internet access.
#[tokio::test]
async fn network_none_blocks_dns() {
    if !docker_reachable() {
        eprintln!("skipping: docker daemon not reachable");
        return;
    }
    let runner: Arc<dyn SandboxRunner> = Arc::new(
        DockerRunner::discover(baybo_process::ProcessManager::transient()).expect("docker runner"),
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = runner
        .run(SandboxSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                "getent hosts example.com >/dev/null 2>&1 && echo OK || echo BLOCKED".into(),
            ],
            cwd: None,
            workspace_root: tmp.path().to_path_buf(),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(120),
            resource_limits: ResourceLimits::default(),
            filesystem_policy: baybo_sandbox::FilesystemPolicy::default(),
        })
        .await
        .expect("sandboxed run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("BLOCKED"),
        "DNS must fail under NetworkPolicy::None (stdout was {stdout:?})"
    );
}

/// The DETACHED path (`spawn_detached` → `DockerDetachedChild`): a
/// `start_kill` followed by an awaited `wait` must remove the daemon-side
/// container (a SIGKILL of the `docker run` client alone orphans it), so the
/// workload's later write never reaches the workspace bind. Covers the
/// detached kill path the `run()`-based cleanup tests don't reach.
#[tokio::test]
async fn detached_start_kill_then_wait_removes_container() {
    if !docker_reachable() {
        eprintln!("skipping: docker daemon not reachable");
        return;
    }
    let runner: Arc<dyn SandboxRunner> = Arc::new(
        DockerRunner::discover(baybo_process::ProcessManager::transient()).expect("docker runner"),
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("post-kill-marker");
    let marker_in_container = marker.to_string_lossy().to_string();

    let mut child = runner
        .spawn_detached(SandboxSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                format!("sleep 4 && touch {marker_in_container}"),
            ],
            cwd: None,
            workspace_root: tmp.path().to_path_buf(),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(60), // ignored by the detached path
            resource_limits: ResourceLimits::default(),
            filesystem_policy: baybo_sandbox::FilesystemPolicy::default(),
        })
        .await
        .expect("spawn_detached");

    // Let the container start, then kill it well before the `touch` fires.
    tokio::time::sleep(Duration::from_millis(800)).await;
    child.start_kill();
    let _ = child.wait().await;

    // Past when the workload's `sleep 4` would have completed: a surviving
    // container would have written the marker into the workspace bind.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !marker.exists(),
        "container survived detached start_kill+wait: marker `{marker_in_container}` was written",
    );
}
