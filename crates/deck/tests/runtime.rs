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
use baybo_storage::sqlite::{SqliteBlobStore, SqliteDeckCardStore, SqlitePool, SqliteSecretStore};
use baybo_store::{BlobStore, DeckCardRow, DeckCardStore, DeckLayoutEntry, DeckSize};
use chrono::Utc;

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
    blob: Arc<SqliteBlobStore>,
    store: Arc<SqliteDeckCardStore>,
    root: tempfile::TempDir,
}

impl Harness {
    fn scratch_root(&self) -> std::path::PathBuf {
        self.root.path().join("scratch")
    }
}

/// Build the manager against tempdirs; no bun required until a service
/// actually spawns (install / boot with enabled rows).
async fn harness() -> Harness {
    let root = tempfile::tempdir().unwrap();
    let pool = SqlitePool::open_in_memory().await.unwrap();
    let store = Arc::new(SqliteDeckCardStore::new(pool.clone()));
    let blob = Arc::new(
        SqliteBlobStore::open(pool.clone(), root.path().join("blobs"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(SecretVault::new(
        EncryptionKey::new(b"deck-test-master-key-32-bytes!!!".to_vec()).unwrap(),
        Arc::new(SqliteSecretStore::new(pool)),
    ));
    let events = Arc::new(RecordingEvents::default());
    let manager = DeckManager::from_config(DeckManagerConfig {
        store: store.clone(),
        vault,
        events: events.clone(),
        blob: blob.clone(),
        deck_root: root.path().join("deck"),
        scratch_root: root.path().join("scratch"),
    });
    Harness {
        manager,
        events,
        blob,
        store,
        root,
    }
}

async fn harness_or_skip(test: &str) -> Option<Harness> {
    if !bun_usable() {
        eprintln!("skipping {test}: bun not installed");
        return None;
    }
    Some(harness().await)
}

/// Count commits in the deck root's git history touching `pathspec`.
fn git_log_count(deck_root: &Path, pathspec: &str) -> usize {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(deck_root)
        .args(["log", "--oneline", "--", pathspec])
        .output()
        .unwrap();
    assert!(out.status.success(), "git log failed");
    String::from_utf8_lossy(&out.stdout).lines().count()
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

/// A card whose refresh op exercises the ref-first blob plane: two
/// `exec`-produced files streamed into the shared store via `blobPutFile`.
/// Returns the refs so the test can assert they landed.
const BLOB_SERVICE: &str = r#"
export const ops = {
  refresh: async (_p, ctx) => {
    await ctx.exec("printf 'from-exec' > out.bin");
    const filed = await ctx.blobPutFile("out.bin", "application/octet-stream");
    await ctx.exec("printf 'hello deck' > note.txt");
    const noted = await ctx.blobPutFile("note.txt", "text/plain");
    return {
      fileId: filed.blobId, fileSize: filed.size, fileCt: filed.contentType,
      noteId: noted.blobId, noteSize: noted.size, noteCt: noted.contentType,
    };
  },
};
"#;

#[tokio::test]
async fn blob_plane_stores_exec_files() {
    let Some(h) = harness_or_skip("blob_plane_stores_exec_files").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    stage_bundle(staged.path(), BLOB_SERVICE);

    // Install runs the dry-run gate, which invokes the refresh op once — so the
    // blobs are produced under the REAL (pre-minted) card id, and the stored
    // first snapshot carries their refs.
    let card = h.manager.install(staged.path()).await.unwrap();
    let view = h.manager.deck_view().await.unwrap();
    let snap: serde_json::Value = serde_json::from_str(&view.snapshots[0].payload).unwrap();

    assert_eq!(snap["fileCt"], "application/octet-stream");
    assert_eq!(snap["fileSize"], 9); // "from-exec"
    assert_eq!(snap["noteCt"], "text/plain");
    assert_eq!(snap["noteSize"], 10); // "hello deck"

    // Both refs resolve to distinct real bytes in the shared blob store.
    let file_id = snap["fileId"].as_str().unwrap();
    let note_id = snap["noteId"].as_str().unwrap();
    assert_ne!(file_id, note_id);
    assert_eq!(h.blob.get(file_id).await.unwrap(), b"from-exec");
    assert_eq!(h.blob.get(note_id).await.unwrap(), b"hello deck");

    let _ = card;
    h.manager.shutdown().await;
}

#[tokio::test]
async fn purge_reclaims_the_cards_blobs() {
    let Some(h) = harness_or_skip("purge_reclaims_the_cards_blobs").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    stage_bundle(staged.path(), BLOB_SERVICE);
    let card = h.manager.install(staged.path()).await.unwrap();

    // The dry-run gate produced two blobs stamped deck:<card_id>.
    let view = h.manager.deck_view().await.unwrap();
    let snap: serde_json::Value = serde_json::from_str(&view.snapshots[0].payload).unwrap();
    let file_id = snap["fileId"].as_str().unwrap().to_string();
    let note_id = snap["noteId"].as_str().unwrap().to_string();
    let ident = format!("deck:{}", card.id);
    assert_eq!(
        h.blob
            .list_ids_by_uploader(&ident, None)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(h.blob.get(&file_id).await.is_ok());

    // Delete → purge → the card's blobs are reclaimed (its own snapshot is
    // gone, so nothing protects them).
    h.manager.soft_delete(&card.id).await.unwrap();
    h.manager.purge(&card.id).await.unwrap();
    assert!(
        h.blob
            .list_ids_by_uploader(&ident, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(h.blob.get(&file_id).await.is_err());
    assert!(h.blob.get(&note_id).await.is_err());

    h.manager.shutdown().await;
}

const TMUX_DIR_SERVICE: &str = r#"
export const ops = {
  refresh: async (_p, ctx) => {
    const r = await ctx.exec('printf %s "$BAYBO_DECK_TMUX_DIR"');
    return { ok: true, tmuxDir: (r.stdout || "").trim() };
  },
  add: async ({ a, b }) => ({ sum: a + b }),
};
"#;

/// Refresh seeds a fake `.sock` into the gate's tmux dir, standing in for
/// the tmux server a real interactive-CLI card would leave running.
const TMUX_SEEDING_SERVICE: &str = r#"
export const ops = {
  refresh: async (_p, ctx) => {
    await ctx.exec('mkdir -p "$BAYBO_DECK_TMUX_DIR" && touch "$BAYBO_DECK_TMUX_DIR/fake.sock"');
    return { ok: true };
  },
  add: async ({ a, b }) => ({ sum: a + b }),
};
"#;

/// Same seeding, then a null snapshot — the gate's failure outcome.
const TMUX_SEEDING_NULL_SERVICE: &str = r#"
export const ops = {
  refresh: async (_p, ctx) => {
    await ctx.exec('mkdir -p "$BAYBO_DECK_TMUX_DIR" && touch "$BAYBO_DECK_TMUX_DIR/fake.sock"');
    return null;
  },
  add: async ({ a, b }) => ({ sum: a + b }),
};
"#;

/// Names under `dir` carrying the gate prefix (empty when `dir` is absent).
fn gate_entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("gate-"))
                .collect()
        })
        .unwrap_or_default()
}

/// Backdate a path's mtime far past the boot sweep's age threshold.
fn make_old(path: &Path) {
    let out = std::process::Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "touch failed: {out:?}");
}

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

    // Install auto-committed the bundle into the deck root's git history.
    assert_eq!(git_log_count(h.manager.deck_root(), &card.id), 1);

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
    // Purge removed the files but git history survives (install + purge),
    // so a purged card's code stays recoverable.
    assert_eq!(git_log_count(h.manager.deck_root(), &card.id), 2);

    h.manager.shutdown().await;
}

/// Every `ctx.exec` is handed `BAYBO_DECK_TMUX_DIR` =
/// `<deck_root>/tmux-socks/<card_id>` — the card's own private-socket dir a
/// tmux-driving card pins its session onto (off the user's default
/// `/tmp/tmux-<uid>/default` socket, out of `/tmp`, and reapable per card
/// at purge).
#[tokio::test]
async fn exec_sees_injected_per_card_tmux_socket_dir() {
    let Some(h) = harness_or_skip("exec_sees_injected_per_card_tmux_socket_dir").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    stage_bundle(staged.path(), TMUX_DIR_SERVICE);

    let card = h.manager.install(staged.path()).await.unwrap();
    let expected = h
        .manager
        .deck_root()
        .join("tmux-socks")
        .join(&card.id)
        .to_string_lossy()
        .into_owned();

    // The resident service's exec runs under the real card id.
    let got = h
        .manager
        .call_op(&card.id, "refresh", json!({}))
        .await
        .unwrap();
    assert_eq!(
        got["tmuxDir"].as_str(),
        Some(expected.as_str()),
        "exec should see BAYBO_DECK_TMUX_DIR={expected}"
    );

    // The dry-run gate's exec ran under its throwaway gate id — still a
    // per-card subdir, never the shared root.
    let view = h.manager.deck_view().await.unwrap();
    assert!(
        view.snapshots[0].payload.contains("tmux-socks/gate-"),
        "gate snapshot {:?} should carry a gate-scoped tmux dir",
        view.snapshots[0].payload
    );

    h.manager.shutdown().await;
}

/// The cross-chat update path: a card installed with no in-context memory
/// of its bundle is discoverable (`deck_view`), its source is retrievable
/// (`bundle_files`, what `DeckCardGet` returns), and an edit built from
/// that source updates it — with git recording both revisions.
#[tokio::test]
async fn update_from_persisted_bundle_round_trips() {
    let Some(h) = harness_or_skip("update_from_persisted_bundle_round_trips").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    stage_bundle(staged.path(), GOOD_SERVICE);
    let card = h.manager.install(staged.path()).await.unwrap();

    // Discovery: a fresh chat resolves title -> card_id via the list.
    let listed = h.manager.deck_view().await.unwrap();
    let found = listed.cards.iter().find(|c| c.title == "Quota").unwrap();
    assert_eq!(found.id, card.id);

    // Fetch the real source (DeckCardGet) — all four files come back.
    let files = h.manager.bundle_files(&card.id).await.unwrap();
    assert!(files.service_js.contains("sum: a + b"));
    assert!(files.manifest_json.contains("Quota"));
    assert!(files.card_html.contains("<div"));
    assert!(files.openapi_json.contains("refresh"));

    // Edit from that source: re-stage the fetched files, change only the
    // service, and update.
    let edit = tempfile::tempdir().unwrap();
    std::fs::write(edit.path().join("manifest.json"), &files.manifest_json).unwrap();
    std::fs::write(edit.path().join("openapi.json"), &files.openapi_json).unwrap();
    std::fs::write(edit.path().join("card.html"), &files.card_html).unwrap();
    std::fs::write(
        edit.path().join("service.js"),
        r#"
export const ops = {
  refresh: async () => ({ ok: true, n: 99 }),
  add: async ({ a, b }) => ({ sum: a + b }),
};
"#,
    )
    .unwrap();
    let updated = h.manager.update(&card.id, edit.path()).await.unwrap();
    assert_eq!(updated.id, card.id, "same card, in place");
    assert_ne!(updated.spec_hash, card.spec_hash, "new code, new hash");

    // The new snapshot reflects the edit, and git holds both revisions —
    // the old source is still recoverable.
    let view = h.manager.deck_view().await.unwrap();
    assert!(view.snapshots[0].payload.contains("99"));
    assert_eq!(git_log_count(h.manager.deck_root(), &card.id), 2);

    h.manager.shutdown().await;
}

/// A layout write may only persist a size the card implements. The gateway
/// checks the token parses, not membership, so the manager clamps an off-set
/// size back to the card's current (valid) size — the `size ∈ sizes`
/// invariant holds on the write path, not just install/update/boot.
#[tokio::test]
async fn set_layout_clamps_size_to_the_cards_implemented_set() {
    let Some(h) = harness_or_skip("set_layout_clamps_size_to_the_cards_implemented_set").await
    else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    stage_bundle(staged.path(), GOOD_SERVICE);
    // A card that implements only wide + large.
    std::fs::write(
        staged.path().join("manifest.json"),
        json!({
            "title": "Quota", "size": "wide", "sizes": ["wide", "large"],
            "refresh": {"op": "refresh", "min_emit_interval_secs": 60}
        })
        .to_string(),
    )
    .unwrap();
    let card = h.manager.install(staged.path()).await.unwrap();
    assert_eq!(card.size, DeckSize::Wide);

    // small is not implemented → clamped to the current valid size.
    h.manager
        .set_layout(&[DeckLayoutEntry {
            id: card.id.clone(),
            position: 0,
            size: DeckSize::Small,
        }])
        .await
        .unwrap();
    let off_set = h.manager.deck_view().await.unwrap();
    assert_eq!(
        off_set.cards.iter().find(|c| c.id == card.id).unwrap().size,
        DeckSize::Wide,
        "off-set size clamped to current"
    );

    // large IS implemented → honored.
    h.manager
        .set_layout(&[DeckLayoutEntry {
            id: card.id.clone(),
            position: 0,
            size: DeckSize::Large,
        }])
        .await
        .unwrap();
    let in_set = h.manager.deck_view().await.unwrap();
    assert_eq!(
        in_set.cards.iter().find(|c| c.id == card.id).unwrap().size,
        DeckSize::Large,
        "in-set size honored"
    );

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

fn card_row(id: &str) -> DeckCardRow {
    DeckCardRow {
        id: id.to_string(),
        title: format!("Card {id}"),
        position: 0,
        size: DeckSize::Wide,
        sizes: vec![DeckSize::Wide],
        maximize: false,
        enabled: false,
        quarantined_at: None,
        deleted_at: None,
        spec_hash: "hash".to_string(),
        last_seq: 0,
        created_at: Utc::now(),
    }
}

/// Purge reaps the card's runtime residue — its private tmux socket dir
/// (a stale `.sock` file stands in for a dead server; `kill-server`
/// against it fails and is ignored) and its exec scratch dir — while
/// another card's residue survives. No bun needed: rows are seeded
/// directly and purge never spawns a service.
#[tokio::test]
async fn purge_reaps_tmux_sockets_and_scratch() {
    let h = harness().await;
    let deck_root = h.manager.deck_root().to_path_buf();
    let scratch_root = h.scratch_root();

    for id in ["card-a", "card-b"] {
        h.store.create(&card_row(id)).await.unwrap();
        let bundle = deck_root.join(id);
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("service.js"), "x").unwrap();
        let socks = deck_root.join("tmux-socks").join(id);
        std::fs::create_dir_all(&socks).unwrap();
        std::fs::write(socks.join("cli.sock"), "").unwrap();
        let scratch = scratch_root.join(id);
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("residue.txt"), "x").unwrap();
    }

    h.store
        .set_deleted("card-a", Some(Utc::now()))
        .await
        .unwrap();
    h.manager.purge("card-a").await.unwrap();

    assert!(!deck_root.join("card-a").exists(), "bundle removed");
    assert!(
        !deck_root.join("tmux-socks").join("card-a").exists(),
        "socket dir reaped"
    );
    assert!(!scratch_root.join("card-a").exists(), "scratch reaped");
    assert!(deck_root.join("card-b").exists(), "other bundle intact");
    assert!(
        deck_root.join("tmux-socks").join("card-b").exists(),
        "other socket dir intact"
    );
    assert!(scratch_root.join("card-b").exists(), "other scratch intact");
}

/// The dry-run gate is hermetic: a tmux-driving card's refresh op pins a
/// socket into the gate-scoped `tmux-socks/gate-<uuid>` dir, and the gate
/// reaps it — servers killed, dir removed, scratch removed — on BOTH
/// outcomes, so install/update/enable/boot re-gates can't accrete one
/// fresh tmux server per run.
#[tokio::test]
async fn dry_run_gate_reaps_its_tmux_socket_dir() {
    let Some(h) = harness_or_skip("dry_run_gate_reaps_its_tmux_socket_dir").await else {
        return;
    };
    let staged = tempfile::tempdir().unwrap();
    stage_bundle(staged.path(), TMUX_SEEDING_SERVICE);
    let card = h.manager.install(staged.path()).await.unwrap();
    let tmux_root = h.manager.deck_root().join("tmux-socks");
    assert_eq!(
        gate_entries(&tmux_root),
        Vec::<String>::new(),
        "pass outcome: gate tmux dir reaped"
    );
    assert_eq!(
        gate_entries(&h.scratch_root()),
        Vec::<String>::new(),
        "pass outcome: gate scratch reaped"
    );

    // Failure outcome: the op seeds a sock, then returns a null snapshot —
    // the gate rejects the bundle but still reaps its runtime.
    let restaged = tempfile::tempdir().unwrap();
    stage_bundle(restaged.path(), TMUX_SEEDING_NULL_SERVICE);
    assert!(h.manager.update(&card.id, restaged.path()).await.is_err());
    assert_eq!(
        gate_entries(&tmux_root),
        Vec::<String>::new(),
        "fail outcome: gate tmux dir reaped"
    );
    assert_eq!(
        gate_entries(&h.scratch_root()),
        Vec::<String>::new(),
        "fail outcome: gate scratch reaped"
    );

    h.manager.shutdown().await;
}

/// Boot's orphan sweep reaps runtime dirs (tmux socket dirs + exec
/// scratch) that belong to no existing card row — gate residue from a
/// crash mid-gate, per-card residue from a crash mid-purge — but only past
/// the age threshold (boot races the live server), and residue of existing
/// rows survives whether live or recycled (a soft-deleted card keeps its
/// runtime residue for restore).
#[tokio::test]
async fn boot_reaps_only_stale_orphan_runtime() {
    let h = harness().await;
    let scratch_root = h.scratch_root();
    let tmux_root = h.manager.deck_root().join("tmux-socks");

    h.store.create(&card_row("card-live")).await.unwrap();
    h.store.create(&card_row("card-bin")).await.unwrap();
    h.store
        .set_deleted("card-bin", Some(Utc::now()))
        .await
        .unwrap();

    let seed = |root: &Path, name: &str, old: bool| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cli.sock"), "").unwrap();
        if old {
            make_old(&dir);
        }
    };
    for (name, old) in [
        ("gate-dead", true),
        ("gate-fresh", false),
        ("card-live", true),
        ("card-bin", true),
        ("orphan-x", true),
        ("orphan-fresh", false),
    ] {
        seed(&tmux_root, name, old);
        seed(&scratch_root, name, old);
    }

    h.manager.boot().await;

    for (root, label) in [(&tmux_root, "tmux"), (&scratch_root, "scratch")] {
        assert!(
            !root.join("gate-dead").exists(),
            "{label}: stale gate residue reaped"
        );
        assert!(
            !root.join("orphan-x").exists(),
            "{label}: stale rowless residue reaped"
        );
        assert!(
            root.join("gate-fresh").exists(),
            "{label}: fresh gate survives the boot race guard"
        );
        assert!(
            root.join("orphan-fresh").exists(),
            "{label}: fresh dir survives the boot race guard"
        );
        assert!(
            root.join("card-live").exists(),
            "{label}: live card residue kept"
        );
        assert!(
            root.join("card-bin").exists(),
            "{label}: recycled card residue kept"
        );
    }
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
