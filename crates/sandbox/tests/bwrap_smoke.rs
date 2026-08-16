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
    let runner: Arc<dyn SandboxRunner> = Arc::new(
        BwrapRunner::discover(baybo_process::ProcessManager::transient())
            .await
            .expect("bwrap runner"),
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
            timeout: Duration::from_secs(5),
            resource_limits: ResourceLimits::unlimited(),
            filesystem_policy: baybo_sandbox::FilesystemPolicy::default(),
        })
        .await
        .expect("sandboxed run");
    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
}

#[tokio::test]
async fn the_live_workspace_key_material_is_masked_inside_a_home_that_contains_it() {
    const KEY_MATERIAL: &str = "AGENT-SECRET-KEY-MATERIAL";
    if !bwrap_present() {
        eprintln!("skipping: bwrap not installed on $PATH");
        return;
    }
    let home = tempfile::tempdir().expect("home tempdir");
    let home_path = home.path().canonicalize().expect("canonical home");
    let live_root = home_path.join("baybo-workspace");
    let key_file = live_root.join(".key").join("agent.key");
    std::fs::create_dir_all(key_file.parent().expect("key dir")).expect("mkdir .key");
    std::fs::write(&key_file, KEY_MATERIAL).expect("write key material");
    let work = live_root.join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");

    let runner: Arc<dyn SandboxRunner> = Arc::new(
        BwrapRunner::discover(baybo_process::ProcessManager::transient())
            .await
            .expect("bwrap runner"),
    );
    let read_the_key = |state_roots: &[PathBuf]| SandboxSpec {
        program: PathBuf::from("/bin/cat"),
        args: vec![key_file.display().to_string()],
        cwd: None,
        workspace_root: work.clone(),
        readable_paths: vec![],
        writable_paths: vec![],
        allowed_hosts: BTreeSet::new(),
        network_policy: NetworkPolicy::None,
        env: EnvPolicy::Baseline,
        stdin: StdinSource::Null,
        timeout: Duration::from_secs(10),
        resource_limits: ResourceLimits::unlimited(),
        filesystem_policy: baybo_sandbox::FilesystemPolicy::Permissive {
            extra_root: home_path.clone(),
            // Missing tmpfs targets are hard bwrap errors.
            denied_paths: baybo_sandbox::default_sensitive_denylist(Some(&home_path), state_roots)
                .into_iter()
                .filter_map(|p| p.canonicalize().ok())
                .collect(),
        },
    };

    let leaked = runner
        .run(read_the_key(&[home_path.join(".baybo")]))
        .await
        .expect("sandboxed run");
    assert_eq!(
        String::from_utf8_lossy(&leaked.stdout),
        KEY_MATERIAL,
        "control: masking only the env-var location leaves the live key readable",
    );

    let masked = runner
        .run(read_the_key(&[live_root.clone(), home_path.join(".baybo")]))
        .await
        .expect("sandboxed run");
    assert_ne!(masked.exit_code, 0, "the key file must not even open");
    assert!(
        !String::from_utf8_lossy(&masked.stdout).contains(KEY_MATERIAL),
        "the live workspace's key material reached a sandboxed command",
    );
}
