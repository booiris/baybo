//! `DeckManager` — the facade the gateway and the deck tools consume.
//!
//! Owns the store, the service supervisor, the dry-run gate, and the
//! event hooks. Every transition into the running fleet — install,
//! update, enable (including from quarantine), restore, and the
//! post-upgrade boot re-run — passes the dry-run gate first: static
//! validation, a real sandboxed boot, one refresh-op invocation, and a
//! checked first snapshot, all before the card is enabled or broadcast.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::Value;

use baybo_security::SecretVault;
use baybo_store::{
    BlobStore, DeckCardRow, DeckCardStore, DeckLayoutEntry, DeckSize, DeckSnapshotRow,
};

use crate::bundle::{
    self, CARD_FILE, DeckBundle, MANIFEST_FILE, MAX_SOURCE_BYTES, MAX_SRC_TOTAL_BYTES,
    OPENAPI_FILE, SDK_VERSION, SERVICE_FILE, SRC_DIR, load_bundle,
};
use crate::error::{DeckError, Result};
use crate::host::DeckHost;
use crate::service::{EmitSink, SNAPSHOT_MAX_BYTES, StrikeRecorder, spawn_service};
use crate::spec::CardSpec;
use crate::supervisor::{DeckSupervisor, QuarantineSink};

/// Hard cap on live (non-deleted) cards; installs and restores past it
/// are refused.
pub const MAX_CARDS: usize = 64;

/// Deck push hooks; the gateway broadcasts `Frame::DeckCardData` /
/// `Frame::DeckChanged` on the owner channel from these.
pub trait DeckEvents: Send + Sync + 'static {
    fn card_data(&self, card_id: &str, seq: i64, payload: &str);
    fn deck_changed(&self);
}

/// Inert hook for tests and headless assemblies.
pub struct NoopDeckEvents;

impl DeckEvents for NoopDeckEvents {
    fn card_data(&self, _card_id: &str, _seq: i64, _payload: &str) {}
    fn deck_changed(&self) {}
}

/// Card provenance / lifecycle transition record. Emitted as structured
/// tracing events under the `deck::provenance` target — the greppable
/// audit spine for "what code ran when" (install / update with hash
/// before→after / delete / restore / purge / quarantine).
fn provenance(event: &str, card_id: &str, detail: &str) {
    tracing::info!(target: "deck::provenance", card = %card_id, event = %event, detail = %detail, "deck provenance");
}

#[derive(Debug, Clone)]
pub struct CardView {
    pub id: String,
    pub title: String,
    pub position: i64,
    pub size: DeckSize,
    /// The grid sizes the card implements (always contains `size`); a
    /// single-entry list is a card that never adapted.
    pub sizes: Vec<DeckSize>,
    /// Whether the card declares a maximized layout.
    pub maximize: bool,
    pub enabled: bool,
    pub quarantined_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub spec_hash: String,
    pub last_seq: i64,
    pub created_at: DateTime<Utc>,
    /// Ops whose mandatory `x-baybo-retryable` declaration is `true`.
    /// Clients feed this into their transport replay policy (the phone's
    /// relay leg replays a silent-death call only for a declared op).
    pub retryable_ops: Vec<String>,
}

impl From<DeckCardRow> for CardView {
    fn from(r: DeckCardRow) -> Self {
        Self {
            id: r.id,
            title: r.title,
            position: r.position,
            size: r.size,
            sizes: r.sizes,
            maximize: r.maximize,
            enabled: r.enabled,
            quarantined_at: r.quarantined_at,
            deleted_at: r.deleted_at,
            spec_hash: r.spec_hash,
            last_seq: r.last_seq,
            created_at: r.created_at,
            retryable_ops: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeckView {
    pub cards: Vec<CardView>,
    pub snapshots: Vec<DeckSnapshotRow>,
}

/// A live card's source, verbatim from its installed bundle — the seed for
/// editing a card across chats (`DeckCardGet`). The four required files plus
/// any `src/` pre-build sources kept alongside them (relative path →
/// contents, e.g. `src/card.tsx`), so an agent editing a `bun build`-authored
/// card gets the real inputs, not just the built `card.html`.
#[derive(Debug, Clone)]
pub struct BundleFiles {
    pub manifest_json: String,
    pub openapi_json: String,
    pub service_js: String,
    pub card_html: String,
    pub src: std::collections::BTreeMap<String, String>,
}

/// Required dependencies, named at the call site (see the repo's
/// `from_config` convention).
pub struct DeckManagerConfig {
    pub store: Arc<dyn DeckCardStore>,
    pub vault: Arc<SecretVault>,
    pub events: Arc<dyn DeckEvents>,
    /// Shared blob store (the same one chat attachments use). Deck-produced
    /// blobs are stamped `deck:<card_id>` so GC can target them without ever
    /// touching chat data. See `docs/modules/deck.md` §Blobs.
    pub blob: Arc<dyn BlobStore>,
    /// `<workspace>/deck` — bundle directories live here.
    pub deck_root: PathBuf,
    /// Scratch root for service + exec working dirs.
    pub scratch_root: PathBuf,
}

struct ManagerEmitSink {
    store: Arc<dyn DeckCardStore>,
    events: Arc<dyn DeckEvents>,
}

#[async_trait]
impl EmitSink for ManagerEmitSink {
    async fn emit(&self, card_id: &str, payload: Value) -> std::result::Result<(), String> {
        let text = payload.to_string();
        if text.len() > SNAPSHOT_MAX_BYTES {
            return Err(format!("emit payload exceeds {SNAPSHOT_MAX_BYTES} bytes"));
        }
        let seq = self
            .store
            .record_snapshot(card_id, &text, None, Utc::now())
            .await
            .map_err(|e| e.to_string())?;
        self.events.card_data(card_id, seq, &text);
        Ok(())
    }
}

struct ManagerQuarantine {
    store: Arc<dyn DeckCardStore>,
    events: Arc<dyn DeckEvents>,
}

#[async_trait]
impl QuarantineSink for ManagerQuarantine {
    async fn quarantine(&self, card_id: &str, reason: &str) {
        provenance("quarantine", card_id, reason);
        let now = Utc::now();
        if let Err(e) = self.store.set_enabled(card_id, false).await {
            tracing::warn!(card = %card_id, "deck: quarantine enable-flip failed: {e}");
        }
        if let Err(e) = self.store.set_quarantined(card_id, Some(now)).await {
            tracing::warn!(card = %card_id, "deck: quarantine stamp failed: {e}");
        }
        // Leave the reason as the card's visible latest state.
        if let Err(e) = self
            .store
            .record_snapshot(card_id, "", Some(reason), now)
            .await
        {
            tracing::warn!(card = %card_id, "deck: quarantine snapshot failed: {e}");
        }
        self.events.deck_changed();
    }
}

pub struct DeckManager {
    store: Arc<dyn DeckCardStore>,
    events: Arc<dyn DeckEvents>,
    /// Shared blob store — for reclaiming a card's `deck:<id>` blobs at purge
    /// (the janitor sweeps live-card garbage separately). See §Blobs.
    blob: Arc<dyn BlobStore>,
    deck_root: PathBuf,
    scratch_root: PathBuf,
    host: Arc<DeckHost>,
    supervisor: Arc<DeckSupervisor>,
    /// Compiled admission contracts keyed by (card_id → (spec_hash, spec)).
    spec_cache: Mutex<HashMap<String, (String, Arc<CardSpec>)>>,
    /// Serializes the bundle git commits (`deck_root` version history) so
    /// two concurrent mutations can't collide on `.git/index.lock`.
    git_lock: tokio::sync::Mutex<()>,
}

impl DeckManager {
    pub fn from_config(config: DeckManagerConfig) -> Arc<Self> {
        let DeckManagerConfig {
            store,
            vault,
            events,
            blob,
            deck_root,
            scratch_root,
        } = config;
        // Card services run on the host (no sandbox), so the runtime is
        // always available — a missing `bun` surfaces as a spawn error at
        // install/boot, not a silent CRUD-only degradation.
        let host = Arc::new(DeckHost::new(vault, scratch_root.clone(), blob.clone()));
        let supervisor = Arc::new(DeckSupervisor::new(
            host.clone(),
            Arc::new(ManagerEmitSink {
                store: store.clone(),
                events: events.clone(),
            }),
            Arc::new(ManagerQuarantine {
                store: store.clone(),
                events: events.clone(),
            }),
            scratch_root.clone(),
        ));

        Arc::new(Self {
            store,
            events,
            blob,
            deck_root,
            scratch_root,
            host,
            supervisor,
            spec_cache: Mutex::new(HashMap::new()),
            git_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn deck_root(&self) -> &Path {
        &self.deck_root
    }

    fn supervisor(&self) -> &Arc<DeckSupervisor> {
        &self.supervisor
    }

    fn bundle_dir(&self, card_id: &str) -> PathBuf {
        self.deck_root.join(card_id)
    }

    // ---- read surface -------------------------------------------------

    pub async fn deck_view(&self) -> Result<DeckView> {
        let mut cards: Vec<CardView> = self
            .store
            .list_live()
            .await?
            .into_iter()
            .map(CardView::from)
            .collect();
        for card in &mut cards {
            // Best-effort: a bundle whose spec no longer parses paints as
            // no-retryable-ops rather than failing the whole view.
            if let Ok(spec) = self.spec_for(&card.id).await {
                card.retryable_ops = spec.retryable_ops();
            }
        }
        let snapshots = self.store.latest_snapshots().await?;
        Ok(DeckView { cards, snapshots })
    }

    pub async fn recycle_view(&self) -> Result<Vec<CardView>> {
        Ok(self
            .store
            .list_deleted()
            .await?
            .into_iter()
            .map(CardView::from)
            .collect())
    }

    pub async fn card_html(&self, card_id: &str) -> Result<String> {
        self.live_row(card_id).await?;
        Ok(std::fs::read_to_string(
            self.bundle_dir(card_id).join(CARD_FILE),
        )?)
    }

    /// The four source files of a live card, read from its installed
    /// bundle. This is what lets the agent edit a card it didn't create —
    /// in a brand-new chat it has no memory of the bundle and its file
    /// tools can't reach the deck root, so the current source has to come
    /// back through a tool result to seed the next `update`.
    pub async fn bundle_files(&self, card_id: &str) -> Result<BundleFiles> {
        self.live_row(card_id).await?;
        let dir = self.bundle_dir(card_id);
        let read = |name: &str| -> Result<String> { Ok(std::fs::read_to_string(dir.join(name))?) };
        Ok(BundleFiles {
            manifest_json: read(MANIFEST_FILE)?,
            openapi_json: read(OPENAPI_FILE)?,
            service_js: read(SERVICE_FILE)?,
            card_html: read(CARD_FILE)?,
            src: read_src_tree(&dir.join(SRC_DIR)),
        })
    }

    pub async fn openapi_json(&self, card_id: &str) -> Result<Value> {
        let spec = self.spec_for(card_id).await?;
        Ok(spec.raw().clone())
    }

    // ---- op calls -----------------------------------------------------

    /// One validated, gateway-crossing op call (the on-demand tap path).
    pub async fn call_op(&self, card_id: &str, op: &str, params: Value) -> Result<Value> {
        let row = self.live_row(card_id).await?;
        if !row.enabled {
            return Err(DeckError::ServiceUnavailable("card is disabled".into()));
        }
        let spec = self.spec_for(card_id).await?;
        spec.validate_call(op, &params)?;
        self.supervisor().call(card_id, op, params).await
    }

    // ---- lifecycle ----------------------------------------------------

    /// Install a validated bundle from `staged_dir` (the agent's scratch)
    /// as a brand-new card. Runs the dry-run gate against the staged
    /// bundle, copies it under the deck root (stage-then-rename), inserts
    /// the row, stores the gate's first snapshot, starts the service, and
    /// broadcasts. Returns the new card.
    pub async fn install(&self, staged_dir: &Path) -> Result<CardView> {
        let live = self.store.count_live().await? as usize;
        if live >= MAX_CARDS {
            return Err(DeckError::DeckFull(MAX_CARDS));
        }
        let bundle = load_bundle(staged_dir)?;
        // Mint the card id BEFORE the gate so a blob the gate's refresh op
        // produces is stamped `deck:<real-id>` (reclaimable), not a throwaway
        // gate identity that would leak. See `docs/modules/deck.md` §Blobs.
        let card_id = uuid::Uuid::new_v4().to_string();
        let first = self.dry_run(&bundle, &card_id).await?;

        let dest = self.bundle_dir(&card_id);
        self.materialize(staged_dir, &card_id, &dest)?;
        // Reload from the final dir so spec_hash covers the stamped manifest.
        let installed = load_bundle(&dest)?;

        let position = self
            .store
            .list_live()
            .await?
            .iter()
            .map(|c| c.position)
            .max()
            .unwrap_or(-1)
            + 1;
        let row = DeckCardRow {
            id: card_id.clone(),
            title: installed.manifest.title.clone(),
            position,
            size: installed.manifest.size,
            sizes: installed.sizes.clone(),
            maximize: installed.maximize,
            enabled: true,
            quarantined_at: None,
            deleted_at: None,
            spec_hash: installed.spec_hash.clone(),
            last_seq: 0,
            created_at: Utc::now(),
        };
        self.store.create(&row).await?;
        provenance("install", &card_id, &installed.spec_hash);
        self.commit_bundle(
            &card_id,
            &installed.manifest.title,
            "install",
            &installed.spec_hash,
        )
        .await;

        let text = first.to_string();
        let seq = self
            .store
            .record_snapshot(&card_id, &text, None, Utc::now())
            .await?;
        self.events.card_data(&card_id, seq, &text);

        self.start_service(&card_id, &installed).await?;
        self.events.deck_changed();
        self.row_view(&card_id).await
    }

    /// Replace an existing card's bundle. Preserves the row's title /
    /// size / layout (the row is authoritative post-install); only
    /// `spec_hash` moves. The service restarts on the new code iff the
    /// card is enabled.
    pub async fn update(&self, card_id: &str, staged_dir: &Path) -> Result<CardView> {
        let row = self.live_row(card_id).await?;
        let bundle = load_bundle(staged_dir)?;
        let first = self.dry_run(&bundle, card_id).await?;

        self.supervisor().stop(card_id).await;
        let dest = self.bundle_dir(card_id);
        let backup = self.staging_path(&format!("{card_id}.old"));
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }
        std::fs::rename(&dest, &backup)?;
        if let Err(e) = self.materialize(staged_dir, card_id, &dest) {
            // Roll the old bundle back so the card isn't left dirless.
            let _ = std::fs::rename(&backup, &dest);
            return Err(e);
        }
        std::fs::remove_dir_all(&backup)?;
        self.spec_cache.lock().remove(card_id);

        let installed = load_bundle(&dest)?;
        let detail = format!("{} -> {}", row.spec_hash, installed.spec_hash);
        provenance("update", card_id, &detail);
        // The row keeps the user's size UNLESS the new code dropped it — the
        // capability set (sizes/maximize) is a property of the code, so it is
        // always refreshed, and an orphaned size clamps to the new default.
        let size = clamp_size(row.size, &installed.sizes, installed.manifest.size);
        self.store
            .set_installed(
                card_id,
                &row.title,
                &installed.spec_hash,
                size,
                &installed.sizes,
                installed.maximize,
            )
            .await?;
        self.commit_bundle(card_id, &row.title, "update", &detail)
            .await;

        let text = first.to_string();
        let seq = self
            .store
            .record_snapshot(card_id, &text, None, Utc::now())
            .await?;
        self.events.card_data(card_id, seq, &text);

        if row.enabled {
            self.start_service(card_id, &installed).await?;
        }
        self.events.deck_changed();
        self.row_view(card_id).await
    }

    pub async fn set_layout(&self, entries: &[DeckLayoutEntry]) -> Result<()> {
        // Enforce the `size ∈ sizes` invariant on the write path too: the
        // gateway only checks that a size token parses, not that the card
        // implements it, so a non-conforming client could otherwise persist a
        // size `card.html` doesn't lay out. An off-set entry keeps the card's
        // current (valid) size instead; an unknown id passes through (the
        // store ignores it).
        let rows = self.store.list_live().await?;
        let by_id: HashMap<&str, &DeckCardRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let clamped: Vec<DeckLayoutEntry> = entries
            .iter()
            .map(|e| {
                let size = match by_id.get(e.id.as_str()) {
                    Some(row) if !row.sizes.contains(&e.size) => row.size,
                    _ => e.size,
                };
                DeckLayoutEntry {
                    id: e.id.clone(),
                    position: e.position,
                    size,
                }
            })
            .collect();
        self.store.set_layout(&clamped).await?;
        self.events.deck_changed();
        Ok(())
    }

    /// Enable: a transition into the running fleet, so it re-passes the
    /// dry-run gate (this is also the quarantine re-admission path — a
    /// failed gate leaves the card quarantined with a refreshed error).
    pub async fn enable(&self, card_id: &str) -> Result<()> {
        let _row = self.live_row(card_id).await?;
        let bundle = load_bundle(&self.bundle_dir(card_id))?;
        match self.dry_run(&bundle, card_id).await {
            Ok(first) => {
                self.store.set_enabled(card_id, true).await?;
                self.store.set_quarantined(card_id, None).await?;
                let text = first.to_string();
                let seq = self
                    .store
                    .record_snapshot(card_id, &text, None, Utc::now())
                    .await?;
                self.events.card_data(card_id, seq, &text);
                self.start_service(card_id, &bundle).await?;
                self.events.deck_changed();
                Ok(())
            }
            Err(e) => {
                let reason = e.to_string();
                self.store
                    .record_snapshot(card_id, "", Some(&reason), Utc::now())
                    .await?;
                self.store
                    .set_quarantined(card_id, Some(Utc::now()))
                    .await?;
                self.events.deck_changed();
                Err(e)
            }
        }
    }

    pub async fn disable(&self, card_id: &str) -> Result<()> {
        self.live_row(card_id).await?;
        self.store.set_enabled(card_id, false).await?;
        self.supervisor().stop(card_id).await;
        self.events.deck_changed();
        Ok(())
    }

    /// Soft delete into the recycle bin: service stopped, row hidden,
    /// bundle files kept.
    pub async fn soft_delete(&self, card_id: &str) -> Result<()> {
        self.live_row(card_id).await?;
        self.supervisor().stop(card_id).await;
        self.store.set_deleted(card_id, Some(Utc::now())).await?;
        provenance("delete", card_id, "soft");
        self.events.deck_changed();
        Ok(())
    }

    /// Restore from the recycle bin. NO dry-run gate, deliberately: the
    /// bundle is byte-identical to when the user deleted it (delete
    /// doesn't touch files) and the user is present and watching — if
    /// the service can't boot (e.g. the SDK moved while it sat in the
    /// bin), the supervisor's crash→quarantine machinery surfaces a
    /// visible error face within seconds, and the Re-enable path IS
    /// gated (`enable` is the re-admission verdict). The gate stays
    /// where new code enters (install/update) and where drift is silent
    /// (the post-upgrade boot re-gate). The card lands with its last
    /// pre-delete snapshot (soft delete keeps them); the resident
    /// service's first tick refreshes it. Counts against the card cap
    /// like an install.
    pub async fn restore(&self, card_id: &str) -> Result<CardView> {
        let row = self
            .store
            .get(card_id)
            .await?
            .ok_or_else(|| DeckError::NotFound(card_id.to_string()))?;
        if row.deleted_at.is_none() {
            return Err(DeckError::Internal("card is not deleted".into()));
        }
        let live = self.store.count_live().await? as usize;
        if live >= MAX_CARDS {
            return Err(DeckError::DeckFull(MAX_CARDS));
        }
        let bundle = load_bundle(&self.bundle_dir(card_id))?;
        self.store.set_deleted(card_id, None).await?;
        self.store.set_quarantined(card_id, None).await?;
        provenance("restore", card_id, &bundle.spec_hash);
        if row.enabled {
            self.start_service(card_id, &bundle).await?;
        }
        self.events.deck_changed();
        self.row_view(card_id).await
    }

    /// Hard delete from the recycle bin: row, snapshots, and bundle files.
    pub async fn purge(&self, card_id: &str) -> Result<()> {
        let row = self
            .store
            .get(card_id)
            .await?
            .ok_or_else(|| DeckError::NotFound(card_id.to_string()))?;
        if row.deleted_at.is_none() {
            return Err(DeckError::Internal(
                "purge only applies to recycled cards; delete it first".into(),
            ));
        }
        self.store.purge(card_id).await?;
        // Reclaim the card's blobs AFTER its snapshots are gone, so its own
        // (now-deleted) refs can't protect them — only another card's live
        // reference does. Best-effort, like provenance/git.
        self.reclaim_card_blobs(card_id).await;
        let dir = self.bundle_dir(card_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        self.spec_cache.lock().remove(card_id);
        provenance("purge", card_id, "hard");
        self.commit_bundle(card_id, &row.title, "purge", "hard")
            .await;
        self.events.deck_changed();
        Ok(())
    }

    /// Boot-time start of every enabled card. A card whose recorded SDK
    /// stamp differs from the current preamble re-passes the gate first
    /// (the post-upgrade re-admission); a gate failure quarantines it
    /// visibly instead of letting it fail on a timer at 3 a.m.
    pub async fn boot(&self) {
        let rows = match self.store.list_live().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("deck: boot listing failed: {e}");
                return;
            }
        };
        for row in rows {
            if !row.enabled || row.quarantined_at.is_some() {
                continue;
            }
            let dir = self.bundle_dir(&row.id);
            let bundle = match load_bundle(&dir) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(card = %row.id, "deck: boot bundle load failed: {e}");
                    self.quarantine_with_reason(&row.id, &e.to_string()).await;
                    continue;
                }
            };
            let needs_regate = bundle.manifest.sdk != Some(SDK_VERSION);
            if needs_regate {
                match self.dry_run(&bundle, &row.id).await {
                    Ok(_) => {
                        if let Err(e) = self.stamp_sdk(&dir) {
                            tracing::warn!(card = %row.id, "deck: sdk stamp failed: {e}");
                        }
                        if let Ok(restamped) = load_bundle(&dir) {
                            let size =
                                clamp_size(row.size, &restamped.sizes, restamped.manifest.size);
                            let _ = self
                                .store
                                .set_installed(
                                    &row.id,
                                    &row.title,
                                    &restamped.spec_hash,
                                    size,
                                    &restamped.sizes,
                                    restamped.maximize,
                                )
                                .await;
                        }
                        provenance("sdk-regate", &row.id, "passed");
                    }
                    Err(e) => {
                        self.quarantine_with_reason(&row.id, &e.to_string()).await;
                        continue;
                    }
                }
            }
            if let Err(e) = self.start_service(&row.id, &bundle).await {
                tracing::warn!(card = %row.id, "deck: boot start failed: {e}");
            }
        }
    }

    pub async fn shutdown(&self) {
        self.supervisor().stop_all().await;
    }

    // ---- internals ----------------------------------------------------

    /// Best-effort: record this bundle-file mutation as a commit in the
    /// deck root's git history. A git failure (no `git`, detached HEAD,
    /// lock contention) is logged and swallowed — version history is a
    /// convenience for the operator, never a correctness dependency of the
    /// deck operation, matching the tracing-based provenance model.
    async fn commit_bundle(&self, card_id: &str, title: &str, event: &str, detail: &str) {
        let _guard = self.git_lock.lock().await;
        match crate::repo::commit_card(&self.deck_root, card_id, title, event, detail).await {
            Ok(Some(sha)) => tracing::debug!(
                target: "deck::provenance",
                card = %card_id, event = %event, %sha, "deck bundle committed"
            ),
            Ok(None) => {}
            Err(reason) => tracing::warn!(
                target: "deck::provenance",
                card = %card_id, event = %event, "deck git commit skipped: {reason}"
            ),
        }
    }

    async fn quarantine_with_reason(&self, card_id: &str, reason: &str) {
        let sink = ManagerQuarantine {
            store: self.store.clone(),
            events: self.events.clone(),
        };
        sink.quarantine(card_id, reason).await;
    }

    async fn live_row(&self, card_id: &str) -> Result<DeckCardRow> {
        let row = self
            .store
            .get(card_id)
            .await?
            .ok_or_else(|| DeckError::NotFound(card_id.to_string()))?;
        if row.deleted_at.is_some() {
            return Err(DeckError::NotFound(card_id.to_string()));
        }
        Ok(row)
    }

    async fn row_view(&self, card_id: &str) -> Result<CardView> {
        let mut view = CardView::from(
            self.store
                .get(card_id)
                .await?
                .ok_or_else(|| DeckError::NotFound(card_id.to_string()))?,
        );
        if view.deleted_at.is_none()
            && let Ok(spec) = self.spec_for(card_id).await
        {
            view.retryable_ops = spec.retryable_ops();
        }
        Ok(view)
    }

    async fn spec_for(&self, card_id: &str) -> Result<Arc<CardSpec>> {
        let row = self.live_row(card_id).await?;
        if let Some((hash, spec)) = self.spec_cache.lock().get(card_id)
            && *hash == row.spec_hash
        {
            return Ok(spec.clone());
        }
        let raw = std::fs::read(self.bundle_dir(card_id).join(OPENAPI_FILE))?;
        let spec = Arc::new(CardSpec::parse(&raw)?);
        self.spec_cache
            .lock()
            .insert(card_id.to_string(), (row.spec_hash, spec.clone()));
        Ok(spec)
    }

    fn staging_path(&self, name: &str) -> PathBuf {
        self.deck_root.join(".staging").join(name)
    }

    /// Copy the bundle from `staged_dir` into a staging dir under the deck
    /// root (same filesystem), stamp the SDK version into the manifest, then
    /// atomically rename into `dest` — SkillInstall's staging discipline.
    /// The four required files are copied verbatim; an optional `src/`
    /// subtree (the card's pre-build sources) rides along under caps so a
    /// `DeckCardGet` can hand the real inputs back for a cross-chat edit.
    fn materialize(&self, staged_dir: &Path, card_id: &str, dest: &Path) -> Result<()> {
        let staging = self.staging_path(card_id);
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        for name in [MANIFEST_FILE, OPENAPI_FILE, SERVICE_FILE, CARD_FILE] {
            std::fs::copy(staged_dir.join(name), staging.join(name))?;
        }
        copy_src_tree(&staged_dir.join(SRC_DIR), &staging.join(SRC_DIR))?;
        self.stamp_sdk(&staging)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&staging, dest)?;
        Ok(())
    }

    fn stamp_sdk(&self, dir: &Path) -> Result<()> {
        let path = dir.join(MANIFEST_FILE);
        let bytes = std::fs::read(&path)?;
        let mut manifest: bundle::DeckManifest = serde_json::from_slice(&bytes)
            .map_err(|e| DeckError::InvalidBundle(format!("{MANIFEST_FILE}: {e}")))?;
        manifest.sdk = Some(SDK_VERSION);
        let out = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| DeckError::Internal(format!("manifest serialize: {e}")))?;
        std::fs::write(&path, out)?;
        Ok(())
    }

    /// Delete a purged card's blobs — service-produced `deck:<id>` and any
    /// picker-uploaded `deck-user:<id>` — skipping any still referenced by
    /// ANOTHER card's retained snapshot. Best-effort: blob GC is a convenience,
    /// never a correctness dependency (a leftover is dead-but-harmless bytes).
    async fn reclaim_card_blobs(&self, card_id: &str) {
        for prefix in [format!("deck:{card_id}"), format!("deck-user:{card_id}")] {
            let ids = match self.blob.list_ids_by_uploader(&prefix, None).await {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(card = %card_id, "deck: blob list for purge failed: {e}");
                    continue;
                }
            };
            for id in ids {
                match self.store.snapshot_references(&id).await {
                    Ok(true) => continue, // another card still points at it
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(blob = %id, "deck: snapshot_references failed: {e}");
                        continue;
                    }
                }
                if let Err(e) = self.blob.delete(&id).await {
                    tracing::warn!(blob = %id, "deck: blob purge delete failed: {e}");
                }
            }
        }
    }

    async fn start_service(&self, card_id: &str, bundle: &DeckBundle) -> Result<()> {
        let sup = self.supervisor();
        sup.start(
            card_id,
            self.bundle_dir(card_id),
            Duration::from_secs(bundle.emit_interval_secs()),
        )
        .await;
        Ok(())
    }

    /// The dry-run gate's execution half: boot the service on the host
    /// against the bundle's own directory, invoke the refresh op once,
    /// and check the returned snapshot. Kills the throwaway process
    /// before returning. Emits during the dry run are discarded.
    ///
    /// `uploader_card_id` is the card's EVENTUAL real id (minted before the
    /// gate on install; the known id on update/enable/boot). The gate's
    /// process/scratch identity is a throwaway `gate-<uuid>` for isolation,
    /// but any blob the refresh op stores must carry the real id so purge/GC
    /// can reclaim it — see the split in [`crate::service::SpawnConfig`].
    async fn dry_run(&self, bundle: &DeckBundle, uploader_card_id: &str) -> Result<Value> {
        let host = self.host.clone();

        struct DiscardEmits;
        #[async_trait]
        impl EmitSink for DiscardEmits {
            async fn emit(
                &self,
                _card_id: &str,
                _payload: Value,
            ) -> std::result::Result<(), String> {
                Ok(())
            }
        }

        let gate_id = format!("gate-{}", uuid::Uuid::new_v4());
        let running = spawn_service(
            crate::service::SpawnConfig {
                card_id: gate_id.clone(),
                uploader_card_id: uploader_card_id.to_string(),
                bundle_dir: bundle.dir.clone(),
                scratch_dir: self.scratch_root.join(&gate_id),
                emit_interval: Duration::from_secs(bundle.emit_interval_secs()),
            },
            host,
            Arc::new(DiscardEmits),
            Arc::new(StrikeRecorder::default()),
        )
        .await?;

        let params = bundle
            .manifest
            .refresh
            .params
            .clone()
            .unwrap_or(Value::Null);
        let outcome = running
            .handle
            .call(&bundle.manifest.refresh.op, params)
            .await;
        let _ = running.kill.send(()).await;
        let scratch = self.scratch_root.join(&gate_id);
        if scratch.exists() {
            let _ = std::fs::remove_dir_all(&scratch);
        }

        let snapshot = outcome.map_err(|e| DeckError::DryRun(format!("refresh op failed: {e}")))?;
        if snapshot.is_null() {
            return Err(DeckError::DryRun(
                "refresh op returned null — a card's refresh must return its snapshot JSON".into(),
            ));
        }
        Ok(snapshot)
    }
}

/// The size correction to apply on an update/boot re-gate: `None` keeps the
/// user's current size (the new code still implements it — and, crucially,
/// leaves the size column alone so a concurrent layout resize survives);
/// `Some(default)` re-clamps to the new manifest default (guaranteed a member
/// of `sizes` by [`crate::bundle::load_bundle`]) when the new code dropped the
/// size the user was on.
fn clamp_size(current: DeckSize, sizes: &[DeckSize], default: DeckSize) -> Option<DeckSize> {
    if sizes.contains(&current) {
        None
    } else {
        Some(default)
    }
}

/// Best-effort read of a card's `src/` subtree into `relative-path → contents`
/// (keys prefixed `src/…`), for `DeckCardGet`. Non-UTF-8 files and anything
/// past the byte caps are skipped rather than failing the read — the required
/// four files are the contract; `src/` is a convenience. Symlinks are ignored
/// (a bundle's `src/` is plain agent-written text).
fn read_src_tree(src_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut total: u64 = 0;
    let mut stack = vec![src_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if meta.len() > MAX_SOURCE_BYTES as u64 || total + meta.len() > MAX_SRC_TOTAL_BYTES {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Key relative to the bundle root (`src/...`), forward slashes.
            let rel = path
                .strip_prefix(src_dir.parent().unwrap_or(src_dir))
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            total += meta.len();
            out.insert(rel, text);
        }
    }
    out
}

/// Copy an optional `src/` subtree into the staging bundle under caps. A
/// missing source dir is a no-op (most cards have no `src/`). Symlinks are
/// refused (an agent must not smuggle `/etc/passwd` into the deck root via a
/// symlinked `src/` entry); the byte caps bound the copy so a stray
/// `node_modules` can't bloat the deck root.
fn copy_src_tree(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    let mut total: u64 = 0;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        std::fs::create_dir_all(&dst)?;
        for entry in std::fs::read_dir(&src)? {
            let entry = entry?;
            let src_path = entry.path();
            let meta = std::fs::symlink_metadata(&src_path)?;
            if meta.file_type().is_symlink() {
                return Err(DeckError::InvalidBundle(format!(
                    "src/ may not contain symlinks: {}",
                    src_path.display()
                )));
            }
            let name = entry.file_name();
            let dst_path = dst.join(&name);
            if meta.is_dir() {
                stack.push((src_path, dst_path));
                continue;
            }
            if meta.len() > MAX_SOURCE_BYTES as u64 {
                return Err(DeckError::InvalidBundle(format!(
                    "src/ file {} exceeds {MAX_SOURCE_BYTES} bytes",
                    src_path.display()
                )));
            }
            total += meta.len();
            if total > MAX_SRC_TOTAL_BYTES {
                return Err(DeckError::InvalidBundle(format!(
                    "src/ subtree exceeds {MAX_SRC_TOTAL_BYTES} bytes"
                )));
            }
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_size_keeps_current_when_still_offered() {
        // `None` = leave the size column alone (don't clobber a concurrent resize).
        assert_eq!(
            clamp_size(
                DeckSize::Small,
                &[DeckSize::Small, DeckSize::Wide],
                DeckSize::Wide
            ),
            None,
        );
    }

    #[test]
    fn clamp_size_falls_to_default_when_dropped() {
        // The user was on `small`, but the new code only offers wide/large.
        assert_eq!(
            clamp_size(
                DeckSize::Small,
                &[DeckSize::Wide, DeckSize::Large],
                DeckSize::Large
            ),
            Some(DeckSize::Large),
        );
    }

    #[test]
    fn src_tree_round_trips_through_copy_and_read() {
        let from = tempfile::tempdir().unwrap();
        let src = from.path().join(SRC_DIR);
        std::fs::create_dir_all(src.join("sizes")).unwrap();
        std::fs::write(src.join("card.tsx"), "export const x = 1;").unwrap();
        std::fs::write(src.join("sizes/max.tsx"), "export const max = 2;").unwrap();

        let to = tempfile::tempdir().unwrap();
        let dst = to.path().join(SRC_DIR);
        copy_src_tree(&src, &dst).unwrap();

        // Keys are bundle-relative with forward slashes.
        let read = read_src_tree(&dst);
        assert_eq!(
            read.get("src/card.tsx").map(String::as_str),
            Some("export const x = 1;")
        );
        assert_eq!(
            read.get("src/sizes/max.tsx").map(String::as_str),
            Some("export const max = 2;")
        );
        assert_eq!(read.len(), 2);
    }

    #[test]
    fn copy_src_tree_is_a_noop_without_a_src_dir() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        let dst = to.path().join(SRC_DIR);
        copy_src_tree(&from.path().join(SRC_DIR), &dst).unwrap();
        assert!(!dst.exists(), "no src/ → nothing copied");
    }

    #[test]
    fn copy_src_tree_refuses_a_symlink() {
        let from = tempfile::tempdir().unwrap();
        let src = from.path().join(SRC_DIR);
        std::fs::create_dir_all(&src).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", src.join("leak.tsx")).unwrap();
        let to = tempfile::tempdir().unwrap();
        let err = copy_src_tree(&src, &to.path().join(SRC_DIR)).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }
}
