//! `DeckManager` — the facade the gateway and the deck tools consume.
//!
//! Owns the store, the service supervisor, the dry-run gate, and the
//! event hooks. Every transition into the running fleet — install,
//! update, enable (including from quarantine), restore, and the
//! post-upgrade boot re-run — passes the dry-run gate first: static
//! validation, a real sandboxed boot, one refresh-op invocation, and a
//! checked first snapshot, all before the card is enabled or broadcast.

use std::collections::{HashMap, HashSet};
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
use crate::service::{EmitSink, RunningService, SNAPSHOT_MAX_BYTES, StrikeRecorder, spawn_service};
use crate::spec::{CardSpec, OpResultKind, RESULT_ERROR_FIELD};
use crate::supervisor::{DeckSupervisor, QuarantineSink};

/// Hard cap on live (non-deleted) cards; installs and restores past it
/// are refused.
pub const MAX_CARDS: usize = 64;

/// Id prefix for dry-run gate runs (`gate-<uuid>`). The gate's runtime
/// dirs — its exec scratch and its private tmux socket dir — carry it;
/// both are reaped when the gate finishes (pass or fail), and anything
/// still carrying the prefix is crash residue for the boot orphan sweep.
const GATE_SCRATCH_PREFIX: &str = "gate-";

/// Wall clock for one best-effort `tmux kill-server` during runtime reap,
/// matching the crate's other bounded subprocess calls; a hung tmux client
/// is logged and skipped, never blocks the lifecycle op.
const TMUX_KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum age (mtime) before the boot orphan sweep may reap a runtime
/// dir. `boot()` runs in a spawned task racing the live server, so a
/// legitimately in-flight gate can already exist at sweep time; only
/// entries old enough to be certain residue are touched.
const BOOT_REAP_MIN_AGE: Duration = Duration::from_secs(60 * 60);
const RESIDUE_REMOVE_MAX_ATTEMPTS: usize = 3;
const RESIDUE_REMOVE_RETRY_DELAY: Duration = Duration::from_millis(20);
const MAX_GATE_ERROR_CHARS: usize = 1_000;

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
/// tracing events under the `baybo_deck::provenance` target — the
/// greppable audit spine for "what code ran when" (install / update with
/// hash before→after / delete / restore / purge / quarantine).
///
/// The `baybo_deck::` prefix is load-bearing, not decoration: the
/// default filter is `baybo=info` and `EnvFilter` matches a directive
/// against the target as a plain prefix, so a target that does not start
/// with `baybo` is dropped on the floor — an audit spine that records
/// nothing while reading, in the source, exactly as if it worked.
const PROVENANCE_TARGET: &str = "baybo_deck::provenance";

fn provenance(event: &str, card_id: &str, detail: &str) {
    tracing::info!(target: PROVENANCE_TARGET, card = %card_id, event = %event, detail = %detail, "deck provenance");
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

#[derive(Debug, Clone)]
pub struct DeckMutationResult {
    pub card: CardView,
    pub first_snapshot: Value,
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
    pub process_manager: Arc<baybo_process::ProcessManager>,
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
    /// Active Baybo config inherited by every service `ctx.exec` child.
    pub baybo_config_path: PathBuf,
}

struct ManagerEmitSink {
    store: Arc<dyn DeckCardStore>,
    events: Arc<dyn DeckEvents>,
}

struct ResultValidatingEmitSink {
    spec: Arc<CardSpec>,
    snapshot_op: String,
    inner: Arc<dyn EmitSink>,
}

#[async_trait]
impl EmitSink for ResultValidatingEmitSink {
    async fn emit(&self, card_id: &str, payload: Value) -> std::result::Result<(), String> {
        if payload.to_string().len() > SNAPSHOT_MAX_BYTES {
            return Err(format!("emit payload exceeds {SNAPSHOT_MAX_BYTES} bytes"));
        }
        self.spec
            .validate_result(&self.snapshot_op, &payload)
            .map_err(|e| format!("invalid snapshot: {e}"))?;
        self.inner.emit(card_id, payload).await
    }

    async fn reject(&self, card_id: &str, reason: &str) {
        self.inner.reject(card_id, reason).await;
    }
}

#[async_trait]
impl EmitSink for ManagerEmitSink {
    async fn emit(&self, card_id: &str, payload: Value) -> std::result::Result<(), String> {
        let text = payload.to_string();
        let seq = self
            .store
            .record_snapshot(card_id, &text, None, Utc::now())
            .await
            .map_err(|e| e.to_string())?;
        self.events.card_data(card_id, seq, &text);
        Ok(())
    }

    async fn reject(&self, card_id: &str, reason: &str) {
        // The same visible shape a quarantine leaves: empty payload, reason
        // in the error column. `card_data` cannot carry it — that frame
        // broadcasts payloads — so the client re-reads on `deck_changed`.
        match self
            .store
            .record_snapshot(card_id, "", Some(reason), Utc::now())
            .await
        {
            Ok(_) => self.events.deck_changed(),
            Err(e) => {
                tracing::warn!(card = %card_id, "deck: rejected-emit snapshot failed: {e}");
            }
        }
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
    process_manager: Arc<baybo_process::ProcessManager>,
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
            process_manager,
            vault,
            events,
            blob,
            deck_root,
            scratch_root,
            baybo_config_path,
        } = config;
        // Card services run on the host (no sandbox), so the runtime is
        // always available — a missing `bun` surfaces as a spawn error at
        // install/boot, not a silent CRUD-only degradation.
        let host = Arc::new(DeckHost::new(
            vault,
            Arc::clone(&process_manager),
            scratch_root.clone(),
            baybo_config_path,
            blob.clone(),
            &deck_root,
        ));
        let supervisor = Arc::new(DeckSupervisor::new(
            host.clone(),
            Arc::new(ManagerQuarantine {
                store: store.clone(),
                events: events.clone(),
            }),
            Arc::clone(&process_manager),
            scratch_root.clone(),
        ));

        Arc::new(Self {
            store,
            events,
            blob,
            deck_root,
            scratch_root,
            process_manager,
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
        let result = self.supervisor().call(card_id, op, params).await?;
        spec.validate_result(op, &result)
            .map_err(|e| DeckError::ServiceUnavailable(format!("invalid service result: {e}")))?;
        Ok(result)
    }

    // ---- lifecycle ----------------------------------------------------

    /// Install a validated bundle from `staged_dir` (the agent's scratch)
    /// as a brand-new card. Runs the dry-run gate against the staged
    /// bundle, copies it under the deck root (stage-then-rename), inserts
    /// the row, stores the gate's first snapshot, starts the service, and
    /// broadcasts. Returns the new card and the checked first snapshot.
    pub async fn install(&self, staged_dir: &Path) -> Result<DeckMutationResult> {
        let live = self.store.count_live().await? as usize;
        if live >= MAX_CARDS {
            return Err(DeckError::DeckFull(MAX_CARDS));
        }
        // Mint the card id BEFORE the gate so a blob the gate's refresh op
        // produces is stamped `deck:<real-id>` (reclaimable), not a throwaway
        // gate identity that would leak. See `docs/modules/deck.md` §Blobs.
        let card_id = uuid::Uuid::new_v4().to_string();
        // Gate → materialize → commit the row is the pre-create window: the gate
        // may stamp `deck:<card_id>` blobs before any row owns them, and purge
        // only reclaims a live/binned card — so a failure here (bad snapshot,
        // timeout, fs/db error) would orphan up to 100 MiB per failed attempt.
        // Reclaim on that path; once the row is committed the card owns the blobs
        // and purge takes over, so post-row failures below do NOT reclaim.
        let (installed, first) = match self.install_commit_row(staged_dir, &card_id).await {
            Ok(v) => v,
            Err(e) => {
                self.reclaim_card_blobs(&card_id).await;
                return Err(e);
            }
        };

        let text = first.to_string();
        let seq = self
            .store
            .record_snapshot(&card_id, &text, None, Utc::now())
            .await?;
        self.events.card_data(&card_id, seq, &text);

        self.start_service(&card_id, &installed).await?;
        self.events.deck_changed();
        let card = self.row_view(&card_id).await?;
        Ok(DeckMutationResult {
            card,
            first_snapshot: first,
        })
    }

    /// The pre-create half of [`Self::install`]: run the dry-run gate,
    /// materialize the bundle under the deck root, and commit the card row.
    /// Returns the reloaded bundle + the gate's first snapshot. It fails iff no
    /// row was committed — the caller reclaims the card's gate-stamped blobs on
    /// exactly that path.
    async fn install_commit_row(
        &self,
        staged_dir: &Path,
        card_id: &str,
    ) -> Result<(DeckBundle, Value)> {
        let bundle = load_bundle(staged_dir)?;
        let first = self.dry_run(&bundle, card_id).await?;

        let dest = self.bundle_dir(card_id);
        self.materialize(staged_dir, card_id, &dest)?;
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
            id: card_id.to_string(),
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
        provenance("install", card_id, &installed.spec_hash);
        self.commit_bundle(
            card_id,
            &installed.manifest.title,
            "install",
            &installed.spec_hash,
        )
        .await;
        Ok((installed, first))
    }

    /// Replace an existing card's bundle. Preserves the row's title /
    /// size / layout (the row is authoritative post-install); only
    /// `spec_hash` moves. The service restarts on the new code iff the
    /// card is enabled.
    pub async fn update(&self, card_id: &str, staged_dir: &Path) -> Result<DeckMutationResult> {
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
        let card = self.row_view(card_id).await?;
        Ok(DeckMutationResult {
            card,
            first_snapshot: first,
        })
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

    /// Hard delete from the recycle bin: row, snapshots, bundle files, and
    /// runtime residue (the card's tmux servers + socket dir, its exec
    /// scratch dir).
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
        // Runtime residue first: the reap never fails, while the bundle
        // removal below can — and once the row is gone a retry returns
        // NotFound, so an fs error here must not strand live tmux servers
        // until the next boot's orphan sweep.
        self.reap_card_runtime(card_id).await;
        // Then reclaim the card's blobs AFTER its snapshots are gone, so its
        // own (now-deleted) refs can't protect them — only another card's live
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

    /// Boot-time start of every enabled card, after the orphan sweep has
    /// reaped runtime residue stranded by crashes. A card whose recorded
    /// SDK stamp differs from the current preamble re-passes the gate
    /// first (the post-upgrade re-admission); a gate failure quarantines
    /// it visibly instead of letting it fail on a timer at 3 a.m.
    pub async fn boot(&self) {
        self.reap_orphan_runtime().await;
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

    /// Best-effort runtime reap shared by purge, the dry-run gate's
    /// finish, and the boot orphan sweep: kill any tmux server the id
    /// pinned into its private socket dir, then drop the socket dir and
    /// the exec scratch dir. Missing residue is the common case and never
    /// fails the caller.
    async fn reap_card_runtime(&self, card_id: &str) {
        let socks = self.host.tmux_dir(card_id);
        kill_tmux_servers(&self.process_manager, &socks).await;
        remove_residue_dir(&socks).await;
        remove_residue_dir(&self.scratch_root.join(card_id)).await;
    }

    /// Boot-time orphan sweep: runtime dirs (tmux socket dirs, exec
    /// scratch) that belong to no existing card row — `gate-*` residue
    /// from a crash mid-gate, per-card residue from a crash mid-purge.
    /// Live and soft-deleted rows both count as existing (a recycled card
    /// keeps its runtime residue for restore). `boot()` races the live
    /// server, so only entries past [`BOOT_REAP_MIN_AGE`] are reaped — a
    /// just-started gate's fresh dirs must survive. A row-listing failure
    /// skips the sweep: without the full id set everything looks orphaned.
    async fn reap_orphan_runtime(&self) {
        let mut existing: HashSet<String> = HashSet::new();
        for rows in [
            self.store.list_live().await,
            self.store.list_deleted().await,
        ] {
            match rows {
                Ok(rows) => existing.extend(rows.into_iter().map(|r| r.id)),
                Err(e) => {
                    tracing::warn!("deck: orphan sweep skipped, row listing failed: {e}");
                    return;
                }
            }
        }
        for dir in stale_orphan_dirs(self.host.tmux_socks_root(), &existing) {
            kill_tmux_servers(&self.process_manager, &dir).await;
            remove_residue_dir(&dir).await;
        }
        for dir in stale_orphan_dirs(&self.scratch_root, &existing) {
            remove_residue_dir(&dir).await;
        }
    }

    /// Best-effort: record this bundle-file mutation as a commit in the
    /// deck root's git history. A git failure (no `git`, detached HEAD,
    /// lock contention) is logged and swallowed — version history is a
    /// convenience for the operator, never a correctness dependency of the
    /// deck operation, matching the tracing-based provenance model.
    async fn commit_bundle(&self, card_id: &str, title: &str, event: &str, detail: &str) {
        let _guard = self.git_lock.lock().await;
        match crate::repo::commit_card(
            &self.process_manager,
            &self.deck_root,
            card_id,
            title,
            event,
            detail,
        )
        .await
        {
            Ok(Some(sha)) => tracing::debug!(
                target: PROVENANCE_TARGET,
                card = %card_id, event = %event, %sha, "deck bundle committed"
            ),
            Ok(None) => {}
            Err(reason) => tracing::warn!(
                target: PROVENANCE_TARGET,
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

    /// Delete a purged card's service-produced blobs (`deck:<card_id>`).
    /// Best-effort — blob GC is a convenience, never a correctness dependency
    /// (a leftover is dead-but-harmless bytes); `delete()`'s own
    /// `any_live_for_path` still spares a content file shared with another live
    /// blob, so only this card's own rows go. This reclaims BOTH the service's
    /// own blobs (`ctx.fetchBlob` / `blobPutFile`) and images a user uploaded
    /// through this card's picker — the gateway stamps both `deck:<card_id>`.
    async fn reclaim_card_blobs(&self, card_id: &str) {
        let prefix = baybo_store::blob::deck_uploader_identity(card_id);
        let ids = match self.blob.list_ids_by_uploader(&prefix, None).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(card = %card_id, "deck: blob list for purge failed: {e}");
                return;
            }
        };
        for id in ids {
            if let Err(e) = self.blob.delete(&id).await {
                tracing::warn!(blob = %id, "deck: blob purge delete failed: {e}");
            }
        }
    }

    async fn start_service(&self, card_id: &str, bundle: &DeckBundle) -> Result<()> {
        let sup = self.supervisor();
        let emit_sink: Arc<dyn EmitSink> = Arc::new(ResultValidatingEmitSink {
            spec: Arc::new(bundle.spec.clone()),
            snapshot_op: bundle.manifest.refresh.op.clone(),
            inner: Arc::new(ManagerEmitSink {
                store: self.store.clone(),
                events: self.events.clone(),
            }),
        });
        sup.start(
            card_id,
            self.bundle_dir(card_id),
            Duration::from_secs(bundle.emit_interval_secs()),
            emit_sink,
        )
        .await;
        Ok(())
    }

    /// The dry-run gate's execution half: boot the service on the host
    /// against the bundle's own directory, invoke the refresh op once,
    /// and check the returned snapshot. Kills the throwaway process
    /// before returning. Emits during the dry run are validated, then discarded.
    ///
    /// `uploader_card_id` is the card's EVENTUAL real id (minted before the
    /// gate on install; the known id on update/enable/boot). The gate's
    /// process/scratch identity is a throwaway `gate-<uuid>` for isolation,
    /// but any blob the refresh op stores must carry the real id so purge/GC
    /// can reclaim it — see the split in [`crate::service::SpawnConfig`].
    ///
    /// Gates are hermetic: whatever runtime the gate's execs left behind —
    /// its scratch dir AND any tmux server pinned into its gate-scoped
    /// `tmux-socks/gate-<uuid>` dir — is reaped on BOTH outcomes, so a
    /// tmux-driving card's every install/update/enable/boot re-gate can't
    /// accrete one fresh tmux server per run (and a dry run never touches
    /// the resident card's server).
    async fn dry_run(&self, bundle: &DeckBundle, uploader_card_id: &str) -> Result<Value> {
        let gate_id = format!("{GATE_SCRATCH_PREFIX}{}", uuid::Uuid::new_v4());
        let outcome = self.dry_run_exec(bundle, &gate_id, uploader_card_id).await;
        self.reap_card_runtime(&gate_id).await;
        let snapshot = outcome?;
        if snapshot.is_null() {
            return Err(DeckError::DryRun(
                "refresh op returned null — a card's refresh must return its snapshot JSON".into(),
            ));
        }
        match bundle
            .spec
            .validate_result(&bundle.manifest.refresh.op, &snapshot)
            .map_err(DeckError::DryRun)?
        {
            OpResultKind::Success => Ok(snapshot),
            OpResultKind::Error => Err(DeckError::DryRun(format!(
                "refresh op returned an error result: {}",
                gate_error_summary(&snapshot)
            ))),
        }
    }

    async fn dry_run_exec(
        &self,
        bundle: &DeckBundle,
        gate_id: &str,
        uploader_card_id: &str,
    ) -> Result<Value> {
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

            async fn reject(&self, _card_id: &str, _reason: &str) {}
        }

        let emit_sink: Arc<dyn EmitSink> = Arc::new(ResultValidatingEmitSink {
            spec: Arc::new(bundle.spec.clone()),
            snapshot_op: bundle.manifest.refresh.op.clone(),
            inner: Arc::new(DiscardEmits),
        });
        let running = spawn_service(
            crate::service::SpawnConfig {
                card_id: gate_id.to_string(),
                uploader_card_id: uploader_card_id.to_string(),
                bundle_dir: bundle.dir.clone(),
                scratch_dir: self.scratch_root.join(gate_id),
                emit_interval: Duration::from_secs(bundle.emit_interval_secs()),
                process_manager: Arc::clone(&self.process_manager),
            },
            self.host.clone(),
            emit_sink,
            Arc::new(StrikeRecorder::default()),
        )
        .await?;

        let params = bundle
            .manifest
            .refresh
            .params
            .clone()
            .unwrap_or(Value::Null);
        let RunningService {
            handle,
            mut exited,
            kill,
        } = running;
        let outcome = handle.call(&bundle.manifest.refresh.op, params).await;
        let _ = kill.send(()).await;
        let _ = (&mut exited).await;
        outcome.map_err(|e| DeckError::DryRun(format!("refresh op failed: {e}")))
    }
}

fn gate_error_summary(snapshot: &Value) -> String {
    let raw = snapshot
        .get(RESULT_ERROR_FIELD)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| snapshot.to_string());
    let mut chars = raw.chars();
    let mut summary: String = chars.by_ref().take(MAX_GATE_ERROR_CHARS).collect();
    if chars.next().is_some() {
        summary.push('…');
    }
    summary
}

/// `tmux -S <sock> kill-server` every eligible `*.sock` entry in `socks`
/// (the server may already be dead, so failures are ignored). Symlinks are
/// skipped — consistent with `copy_src_tree`/`read_src_tree`'s hostile-
/// symlink stance, a planted `x.sock -> /tmp/tmux-<uid>/default` must not
/// aim `kill-server` at the user's own server. Each kill is bounded by
/// [`TMUX_KILL_TIMEOUT`]; a timeout is logged and the sweep continues.
async fn kill_tmux_servers(process_manager: &Arc<baybo_process::ProcessManager>, socks: &Path) {
    let Ok(entries) = std::fs::read_dir(socks) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_killable_sock(&path) {
            continue;
        }
        let mut kill = tokio::process::Command::new("tmux");
        kill.arg("-S")
            .arg(&path)
            .arg("kill-server")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let timed_out = match process_manager.spawn(&mut kill, "deck-tmux-reap") {
            Ok(mut child) => tokio::time::timeout(TMUX_KILL_TIMEOUT, child.wait())
                .await
                .is_err(),
            Err(_) => false,
        };
        if timed_out {
            tracing::warn!(sock = %path.display(), "deck: tmux kill-server timed out; continuing");
        }
    }
}

/// A `kill-server` target must be a non-symlink `*.sock` entry.
fn is_killable_sock(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("sock") {
        return false;
    }
    std::fs::symlink_metadata(path)
        .map(|m| !m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Entries under `root` eligible for the boot orphan sweep: named as gate
/// residue (`gate-*`) or matching no existing card row, AND older than
/// [`BOOT_REAP_MIN_AGE`] (an unreadable mtime counts as fresh — never reap
/// what can't be aged).
fn stale_orphan_dirs(root: &Path, existing: &HashSet<String>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name.starts_with(GATE_SCRATCH_PREFIX) || !existing.contains(&name))
                && older_than(&e.path(), BOOT_REAP_MIN_AGE)
        })
        .map(|e| e.path())
        .collect()
}

fn older_than(path: &Path, min_age: Duration) -> bool {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age >= min_age)
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

/// Remove a runtime-residue dir, tolerating its absence (the common case)
/// and logging — never propagating — anything else: residue cleanup must
/// not fail the lifecycle operation it rides on.
async fn remove_residue_dir(dir: &Path) {
    remove_residue_dir_with(dir, tokio::fs::remove_dir_all).await;
}

async fn remove_residue_dir_with<F, Fut>(dir: &Path, mut remove: F)
where
    F: FnMut(PathBuf) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    for attempt in 1..=RESIDUE_REMOVE_MAX_ATTEMPTS {
        match remove(dir.to_path_buf()).await {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e)
                if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    && attempt < RESIDUE_REMOVE_MAX_ATTEMPTS =>
            {
                tokio::time::sleep(RESIDUE_REMOVE_RETRY_DELAY).await;
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), attempts = attempt, "deck: residue cleanup failed: {e}");
                return;
            }
        }
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
    fn killable_sock_skips_symlinks_and_non_socks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.sock");
        std::fs::write(&real, "").unwrap();
        assert!(is_killable_sock(&real));

        let planted = dir.path().join("planted.sock");
        std::os::unix::fs::symlink(&real, &planted).unwrap();
        assert!(
            !is_killable_sock(&planted),
            "a symlinked .sock must never be a kill-server target"
        );

        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "").unwrap();
        assert!(!is_killable_sock(&txt));
        assert!(!is_killable_sock(&dir.path().join("missing.sock")));
    }

    #[test]
    fn older_than_treats_fresh_entries_as_unreapable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !older_than(dir.path(), BOOT_REAP_MIN_AGE),
            "a just-created dir must survive the boot sweep"
        );
        assert!(older_than(dir.path(), Duration::ZERO));
        assert!(!older_than(&dir.path().join("missing"), Duration::ZERO));
    }

    #[tokio::test]
    async fn residue_cleanup_retries_directory_not_empty() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        remove_residue_dir_with(Path::new("/unused"), |_| {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(if attempt + 1 < RESIDUE_REMOVE_MAX_ATTEMPTS {
                Err(std::io::ErrorKind::DirectoryNotEmpty.into())
            } else {
                Ok(())
            })
        })
        .await;
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::Relaxed),
            RESIDUE_REMOVE_MAX_ATTEMPTS
        );
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

    /// The provenance target must stay under the `baybo` prefix.
    ///
    /// The default filter is `baybo=info` and `EnvFilter` matches a
    /// directive against the target as a plain prefix, so a target of
    /// `deck::provenance` compiles, reads correctly, emits nothing, and
    /// leaves install / delete / quarantine unrecorded — which is
    /// exactly what shipped until a deck outage turned up an audit
    /// trail with zero entries in it.
    #[test]
    fn provenance_target_is_reachable_by_the_default_filter() {
        assert!(
            PROVENANCE_TARGET.starts_with("baybo"),
            "target {PROVENANCE_TARGET:?} is invisible to the default `baybo=info` filter"
        );
    }
}
