//! End-to-end deck runtime smoke: real sandbox backend + real bun.
//!
//! Self-skips when the platform sandbox backend is missing or unusable
//! (e.g. nested sandbox-exec) or bun is not installed — the same
//! self-skip discipline as the sandbox/docker smokes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;

use async_trait::async_trait;
use baybo_deck::{DeckEvents, DeckManager, DeckManagerConfig};
use baybo_sandbox::spec::{Backend, SandboxOutput, SandboxSpec, StdinSource};
use baybo_sandbox::{DetachedChild, SandboxError, SandboxRunner, TokioDetachedChild};
use baybo_security::{EncryptionKey, SecretVault};
use baybo_storage::sqlite::{SqliteDeckCardStore, SqlitePool, SqliteSecretStore};

/// Plain-tokio runner: same process semantics, no OS isolation. Lets the
/// full spawn/stdio/gate pipeline run on hosts whose OS backend is
/// broken (this Mac's sandbox-exec smoke fails environmentally); the
/// isolation layer itself is covered by the sandbox crate's smokes.
struct PlainRunner;

#[async_trait]
impl SandboxRunner for PlainRunner {
    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
        let started = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(spec.cwd.as_deref().unwrap_or(&spec.workspace_root))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(SandboxError::Io)?;
        let out = tokio::time::timeout(spec.timeout, child.wait_with_output())
            .await
            .map_err(|_| SandboxError::Timeout(spec.timeout))?
            .map_err(SandboxError::Io)?;
        Ok(SandboxOutput {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
            elapsed: started.elapsed(),
            timed_out: false,
        })
    }

    async fn spawn_detached(
        &self,
        spec: SandboxSpec,
    ) -> Result<Box<dyn DetachedChild>, SandboxError> {
        let mut cmd = tokio::process::Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(spec.cwd.as_deref().unwrap_or(&spec.workspace_root))
            .stdin(match spec.stdin {
                StdinSource::Piped => std::process::Stdio::piped(),
                _ => std::process::Stdio::null(),
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        Ok(Box::new(TokioDetachedChild(
            cmd.spawn().map_err(SandboxError::Io)?,
        )))
    }

    fn backend(&self) -> Backend {
        Backend::SandboxExec
    }
}

#[derive(Default)]
struct RecordingEvents {
    card_data: Mutex<Vec<(String, i64)>>,
    changed: Mutex<u32>,
}

impl DeckEvents for RecordingEvents {
    fn card_data(&self, card_id: &str, seq: i64, _payload: &str) {
        self.card_data.lock().push((card_id.to_string(), seq));
    }
    fn deck_changed(&self) {
        *self.changed.lock() += 1;
    }
}

async fn sandbox_usable() -> bool {
    let Ok(runner) = baybo_sandbox::current_platform_runner() else {
        return false;
    };
    let tmp = tempfile::tempdir().unwrap();
    let spec = baybo_sandbox::spec::SandboxSpec {
        program: "/usr/bin/true".into(),
        args: vec![],
        cwd: Some(tmp.path().to_path_buf()),
        workspace_root: tmp.path().to_path_buf(),
        readable_paths: vec![],
        writable_paths: vec![],
        allowed_hosts: Default::default(),
        network_policy: baybo_sandbox::spec::NetworkPolicy::None,
        env: baybo_sandbox::spec::EnvPolicy::Baseline,
        stdin: baybo_sandbox::spec::StdinSource::Null,
        timeout: Duration::from_secs(5),
        resource_limits: runner.default_resource_limits(),
        filesystem_policy: baybo_sandbox::spec::FilesystemPolicy::Permissive {
            extra_root: tmp.path().to_path_buf(),
            denied_paths: vec![],
        },
    };
    matches!(runner.run(spec).await, Ok(out) if out.exit_code == 0)
}

fn bun_usable() -> bool {
    std::process::Command::new(std::env::var_os("BAYBO_BUN_BIN").unwrap_or_else(|| "bun".into()))
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct Harness {
    manager: Arc<DeckManager>,
    events: Arc<RecordingEvents>,
    _root: tempfile::TempDir,
}

async fn harness_or_skip(test: &str) -> Option<Harness> {
    if !bun_usable() {
        eprintln!("skipping {test}: bun not installed");
        return None;
    }
    // Prefer the real OS backend; fall back to the plain runner where it
    // is environmentally broken so the pipeline still gets exercised.
    let runner: Arc<dyn SandboxRunner> = if sandbox_usable().await {
        baybo_sandbox::current_platform_runner().expect("probe passed")
    } else {
        eprintln!("{test}: OS sandbox backend unusable here; using plain runner");
        Arc::new(PlainRunner)
    };
    let root = tempfile::tempdir().unwrap();
    let pool = SqlitePool::open_in_memory().await.unwrap();
    let store = Arc::new(SqliteDeckCardStore::new(pool.clone()));
    let vault = Arc::new(SecretVault::new(
        EncryptionKey::new(b"deck-test-master-key-32-bytes!!!".to_vec()).unwrap(),
        Arc::new(SqliteSecretStore::new(pool)),
    ));
    let events = Arc::new(RecordingEvents::default());
    let manager = DeckManager::from_config_with_runner(
        DeckManagerConfig {
            store,
            vault,
            events: events.clone(),
            deck_root: root.path().join("deck"),
            scratch_root: root.path().join("scratch"),
            internal: None,
        },
        Some(runner),
    );
    Some(Harness {
        manager,
        events,
        _root: root,
    })
}

fn stage_bundle(dir: &Path, service_js: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        json!({
            "title": "Quota",
            "size": "wide",
            "refresh": {"op": "refresh", "min_emit_interval_secs": 60}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("openapi.json"),
        json!({
            "openapi": "3.1.0",
            "paths": {
                "/refresh": {"get": {"x-baybo-retryable": true}},
                "/add": {"post": {"x-baybo-retryable": false, "parameters": [
                    {"name": "a", "required": true, "schema": {"type": "integer"}},
                    {"name": "b", "required": true, "schema": {"type": "integer"}}
                ]}}
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("service.js"), service_js).unwrap();
    std::fs::write(dir.join("card.html"), "<div id=x></div>").unwrap();
}

const GOOD_SERVICE: &str = r#"
export const ops = {
  refresh: async () => ({ ok: true, n: 42 }),
  add: async ({ a, b }) => ({ sum: a + b }),
};
"#;

#[tokio::test]
async fn install_call_lifecycle() {
    let Some(h) = harness_or_skip("install_call_lifecycle").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    stage_bundle(staged.path(), GOOD_SERVICE);

    // Install runs the dry-run gate; the first snapshot is stored.
    let card = h.manager.install(staged.path()).await.unwrap();
    assert_eq!(card.title, "Quota");
    assert!(card.enabled);
    let view = h.manager.deck_view().await.unwrap();
    assert_eq!(view.cards.len(), 1);
    assert_eq!(view.snapshots.len(), 1);
    assert!(view.snapshots[0].payload.contains("42"));
    assert!(!h.events.card_data.lock().is_empty());
    assert!(*h.events.changed.lock() >= 1);

    // Validated on-demand op call reaches the resident service.
    let sum = h
        .manager
        .call_op(&card.id, "add", json!({"a": 2, "b": 3}))
        .await
        .unwrap();
    assert_eq!(sum, json!({"sum": 5}));

    // Off-schema calls die at the gateway, never reaching the child.
    assert!(
        h.manager
            .call_op(&card.id, "add", json!({"a": "x", "b": 3}))
            .await
            .is_err()
    );
    assert!(
        h.manager
            .call_op(&card.id, "ghost", json!({}))
            .await
            .is_err()
    );

    // Disable stops the process; enable re-passes the gate.
    h.manager.disable(&card.id).await.unwrap();
    assert!(
        h.manager
            .call_op(&card.id, "add", json!({"a": 1, "b": 1}))
            .await
            .is_err()
    );
    h.manager.enable(&card.id).await.unwrap();
    let sum = h
        .manager
        .call_op(&card.id, "add", json!({"a": 1, "b": 1}))
        .await
        .unwrap();
    assert_eq!(sum, json!({"sum": 2}));

    // Soft delete -> bin -> restore -> purge.
    h.manager.soft_delete(&card.id).await.unwrap();
    assert!(h.manager.deck_view().await.unwrap().cards.is_empty());
    assert_eq!(h.manager.recycle_view().await.unwrap().len(), 1);
    // Purge refuses a live... (card is deleted, so purge is allowed only now)
    h.manager.restore(&card.id).await.unwrap();
    assert_eq!(h.manager.deck_view().await.unwrap().cards.len(), 1);
    assert!(h.manager.purge(&card.id).await.is_err(), "not deleted");
    h.manager.soft_delete(&card.id).await.unwrap();
    h.manager.purge(&card.id).await.unwrap();
    assert!(h.manager.recycle_view().await.unwrap().is_empty());
    assert!(!h.manager.deck_root().join(&card.id).exists());

    h.manager.shutdown().await;
}

#[tokio::test]
async fn dry_run_gate_returns_boot_failures_to_the_agent() {
    let Some(h) = harness_or_skip("dry_run_gate_returns_boot_failures_to_the_agent").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    // Import-time crash: the gate must surface it, and nothing installs.
    stage_bundle(staged.path(), "throw new Error('boom at import');");
    let err = h.manager.install(staged.path()).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("boom at import"), "gate error: {msg}");
    assert!(h.manager.deck_view().await.unwrap().cards.is_empty());

    // A refresh op that returns null is not a snapshot.
    stage_bundle(
        staged.path(),
        "export const ops = { refresh: async () => null };",
    );
    let err = h.manager.install(staged.path()).await.unwrap_err();
    assert!(err.to_string().contains("null"), "{err}");

    h.manager.shutdown().await;
}

#[tokio::test]
async fn service_self_timer_emits_are_policed_and_recorded() {
    let Some(h) = harness_or_skip("service_self_timer_emits_are_policed_and_recorded").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    // start() emits immediately; the manifest floor (clamped to the
    // gateway floor) governs acceptance of the follow-ups.
    stage_bundle(
        staged.path(),
        r#"
export const ops = { refresh: async () => ({ tick: 0 }) };
export function start(ctx) {
  ctx.emit({ tick: 1 });
}
"#,
    );
    let card = h.manager.install(staged.path()).await.unwrap();
    // The start() emit lands as seq 2 (seq 1 is the gate's stored first
    // snapshot). Give the resident service a moment.
    let mut latest = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let view = h.manager.deck_view().await.unwrap();
        if let Some(s) = view.snapshots.first()
            && s.payload.contains("tick")
            && s.seq >= 2
        {
            latest = Some(s.clone());
            break;
        }
    }
    let latest = latest.expect("self-timer emit accepted and recorded");
    assert!(latest.payload.contains("\"tick\":1"));
    // The install-returned view already carries the gate snapshot's seq.
    assert_eq!(card.last_seq, 1);

    h.manager.shutdown().await;
}
