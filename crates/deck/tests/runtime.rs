//! End-to-end deck runtime smoke: real bun running on the host.
//!
//! Card services run directly on the host (no sandbox), so this only
//! needs `bun` on `PATH` (or `BAYBO_BUN_BIN`); it self-skips when bun is
//! not installed.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;

use baybo_deck::{DeckEvents, DeckManager, DeckManagerConfig};
use baybo_security::{EncryptionKey, SecretVault};
use baybo_storage::sqlite::{SqliteDeckCardStore, SqlitePool, SqliteSecretStore};

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
    let root = tempfile::tempdir().unwrap();
    let pool = SqlitePool::open_in_memory().await.unwrap();
    let store = Arc::new(SqliteDeckCardStore::new(pool.clone()));
    let vault = Arc::new(SecretVault::new(
        EncryptionKey::new(b"deck-test-master-key-32-bytes!!!".to_vec()).unwrap(),
        Arc::new(SqliteSecretStore::new(pool)),
    ));
    let events = Arc::new(RecordingEvents::default());
    let manager = DeckManager::from_config(DeckManagerConfig {
        store,
        vault,
        events: events.clone(),
        deck_root: root.path().join("deck"),
        scratch_root: root.path().join("scratch"),
        internal: None,
    });
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
