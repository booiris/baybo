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

use baybo_sandbox::SandboxRunner;
use baybo_security::SecretVault;
use baybo_store::{DeckCardRow, DeckCardStore, DeckLayoutEntry, DeckSize, DeckSnapshotRow};

use crate::bundle::{
    self, CARD_FILE, DeckBundle, MANIFEST_FILE, OPENAPI_FILE, SDK_VERSION, SERVICE_FILE,
    load_bundle,
};
use crate::error::{DeckError, Result};
use crate::host::{DeckHost, InternalReads};
use crate::service::{EmitSink, SNAPSHOT_MAX_BYTES, StrikeRecorder, spawn_service};
use crate::spec::CardSpec;
use crate::supervisor::{DeckSupervisor, QuarantineSink};

/// Hard cap on live (non-deleted) cards; installs and restores past it
/// are refused.
pub const MAX_CARDS: usize = 24;

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

/// Required dependencies, named at the call site (see the repo's
/// `from_config` convention).
pub struct DeckManagerConfig {
    pub store: Arc<dyn DeckCardStore>,
    pub vault: Arc<SecretVault>,
    pub events: Arc<dyn DeckEvents>,
    /// `<workspace>/deck` — bundle directories live here.
    pub deck_root: PathBuf,
    /// Scratch root for service + exec working dirs.
    pub scratch_root: PathBuf,
    /// The curated `baybo://` read registry; `None` disables internal reads.
    pub internal: Option<Arc<dyn InternalReads>>,
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
    deck_root: PathBuf,
    scratch_root: PathBuf,
    /// `None` when no usable sandbox backend exists on this host — the
    /// deck degrades to read-only CRUD (rows + cached snapshots serve;
    /// installs and service calls refuse with a clear error).
    runner: Option<Arc<dyn SandboxRunner>>,
    host: Option<Arc<DeckHost>>,
    supervisor: Option<Arc<DeckSupervisor>>,
    /// Compiled admission contracts keyed by (card_id → (spec_hash, spec)).
    spec_cache: Mutex<HashMap<String, (String, Arc<CardSpec>)>>,
}

impl DeckManager {
    pub fn from_config(config: DeckManagerConfig) -> Arc<Self> {
        let runner = match baybo_sandbox::current_platform_runner() {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(
                    "deck: no usable sandbox backend ({e}); card services are unavailable"
                );
                None
            }
        };
        Self::build(config, runner)
    }

    /// Test-only constructor with an injected runner, so the full
    /// spawn/stdio/gate pipeline is exercisable on hosts whose OS
    /// backend is unusable (the OS-isolation layer has its own smokes).
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_config_with_runner(
        config: DeckManagerConfig,
        runner: Option<Arc<dyn SandboxRunner>>,
    ) -> Arc<Self> {
        Self::build(config, runner)
    }

    fn build(config: DeckManagerConfig, runner: Option<Arc<dyn SandboxRunner>>) -> Arc<Self> {
        let DeckManagerConfig {
            store,
            vault,
            events,
            deck_root,
            scratch_root,
            internal,
        } = config;
        let host = runner.as_ref().map(|r| {
            Arc::new(DeckHost::new(
                vault.clone(),
                r.clone(),
                internal,
                scratch_root.clone(),
            ))
        });
        let supervisor = match (&runner, &host) {
            (Some(runner), Some(host)) => Some(Arc::new(DeckSupervisor::new(
                runner.clone(),
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
            ))),
            _ => None,
        };

        Arc::new(Self {
            store,
            events,
            deck_root,
            scratch_root,
            runner,
            host,
            supervisor,
            spec_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn deck_root(&self) -> &Path {
        &self.deck_root
    }

    fn supervisor(&self) -> Result<&Arc<DeckSupervisor>> {
        self.supervisor.as_ref().ok_or_else(|| {
            DeckError::ServiceUnavailable(
                "no usable sandbox backend on this host (deck services need bwrap or sandbox-exec)"
                    .into(),
            )
        })
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
        self.supervisor()?.call(card_id, op, params).await
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
        let first = self.dry_run(&bundle).await?;

        let card_id = uuid::Uuid::new_v4().to_string();
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
            enabled: true,
            quarantined_at: None,
            deleted_at: None,
            spec_hash: installed.spec_hash.clone(),
            last_seq: 0,
            created_at: Utc::now(),
        };
        self.store.create(&row).await?;
        provenance("install", &card_id, &installed.spec_hash);

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
        let first = self.dry_run(&bundle).await?;

        if let Some(sup) = &self.supervisor {
            sup.stop(card_id).await;
        }
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
        provenance(
            "update",
            card_id,
            &format!("{} -> {}", row.spec_hash, installed.spec_hash),
        );
        self.store
            .set_installed(card_id, &row.title, &installed.spec_hash)
            .await?;

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
        self.store.set_layout(entries).await?;
        self.events.deck_changed();
        Ok(())
    }

    /// Enable: a transition into the running fleet, so it re-passes the
    /// dry-run gate (this is also the quarantine re-admission path — a
    /// failed gate leaves the card quarantined with a refreshed error).
    pub async fn enable(&self, card_id: &str) -> Result<()> {
        let _row = self.live_row(card_id).await?;
        let bundle = load_bundle(&self.bundle_dir(card_id))?;
        match self.dry_run(&bundle).await {
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
        if let Some(sup) = &self.supervisor {
            sup.stop(card_id).await;
        }
        self.events.deck_changed();
        Ok(())
    }

    /// Soft delete into the recycle bin: service stopped, row hidden,
    /// bundle files kept.
    pub async fn soft_delete(&self, card_id: &str) -> Result<()> {
        self.live_row(card_id).await?;
        if let Some(sup) = &self.supervisor {
            sup.stop(card_id).await;
        }
        self.store.set_deleted(card_id, Some(Utc::now())).await?;
        provenance("delete", card_id, "soft");
        self.events.deck_changed();
        Ok(())
    }

    /// Restore from the recycle bin — a transition into the running
    /// fleet, so it re-passes the gate; a failed gate leaves the card in
    /// the bin with the error returned to the caller. Counts against the
    /// card cap like an install.
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
        let first = self.dry_run(&bundle).await?;
        self.store.set_deleted(card_id, None).await?;
        self.store.set_quarantined(card_id, None).await?;
        provenance("restore", card_id, &bundle.spec_hash);
        let text = first.to_string();
        let seq = self
            .store
            .record_snapshot(card_id, &text, None, Utc::now())
            .await?;
        self.events.card_data(card_id, seq, &text);
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
        let dir = self.bundle_dir(card_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        self.spec_cache.lock().remove(card_id);
        provenance("purge", card_id, "hard");
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
                match self.dry_run(&bundle).await {
                    Ok(_) => {
                        if let Err(e) = self.stamp_sdk(&dir) {
                            tracing::warn!(card = %row.id, "deck: sdk stamp failed: {e}");
                        }
                        if let Ok(restamped) = load_bundle(&dir) {
                            let _ = self
                                .store
                                .set_installed(&row.id, &row.title, &restamped.spec_hash)
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
        if let Some(sup) = &self.supervisor {
            sup.stop_all().await;
        }
    }

    // ---- internals ----------------------------------------------------

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

    /// Copy the four bundle files from `staged_dir` into a staging dir
    /// under the deck root (same filesystem), stamp the SDK version into
    /// the manifest, then atomically rename into `dest` — SkillInstall's
    /// staging discipline.
    fn materialize(&self, staged_dir: &Path, card_id: &str, dest: &Path) -> Result<()> {
        let staging = self.staging_path(card_id);
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        for name in [MANIFEST_FILE, OPENAPI_FILE, SERVICE_FILE, CARD_FILE] {
            std::fs::copy(staged_dir.join(name), staging.join(name))?;
        }
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

    async fn start_service(&self, card_id: &str, bundle: &DeckBundle) -> Result<()> {
        let sup = self.supervisor()?;
        sup.start(
            card_id,
            self.bundle_dir(card_id),
            Duration::from_secs(bundle.emit_interval_secs()),
        )
        .await;
        Ok(())
    }

    /// The dry-run gate's execution half: boot the service in the real
    /// sandbox against the bundle's own directory, invoke the refresh op
    /// once, and check the returned snapshot. Kills the throwaway
    /// process before returning. Emits during the dry run are discarded.
    async fn dry_run(&self, bundle: &DeckBundle) -> Result<Value> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| {
                DeckError::ServiceUnavailable(
                    "no usable sandbox backend on this host (deck services need bwrap or sandbox-exec)"
                        .into(),
                )
            })?
            .clone();
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| DeckError::Internal("host services missing".into()))?
            .clone();

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
            &runner,
            crate::service::SpawnConfig {
                card_id: gate_id.clone(),
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
