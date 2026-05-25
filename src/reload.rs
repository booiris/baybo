//! In-process config hot-reload orchestrator.
//!
//! Implements [`aura_gateway::ConfigReloader`]. Lives in the bin crate
//! because rebuilding the LLM pool needs the application boot layer
//! ([`crate::boot::build_llm_client_for_entry`]). A two-phase
//! prepare→commit keeps the swap atomic: the fallible pool rebuild
//! happens in `prepare`, and a failure there aborts before anything is
//! swapped. See `docs/config-hot-reload.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_agent::router::LiveRateLimit;
use aura_agent::service::ShutdownSignal;
use aura_agent::{LlmClientPool, LlmPoolHandle};
use aura_config::{AuraConfig, ConfigHandle, hot_reload_diff};
use aura_cost::{CostManager, SpendingLimits, cost_call_guard};
use aura_gateway::{ConfigReloader, ReloadError, ReloadOutcome};
use aura_llm::{GuardedLlm, LlmProviderRegistry, ModelPricing};
use aura_model::LlmEntryName;
use aura_security::SecretVault;
use aura_store::BlobStore;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::boot;

/// Build one `Arc<GuardedLlm>` per `config.llm` entry, concurrently.
/// Mirrors boot's failure policy exactly: a **default** entry that
/// fails to build is a hard error (the pool would be unusable); a
/// non-default failure is dropped with a `warn!` and its name returned
/// in the second tuple element.
pub(crate) async fn build_pool_clients(
    config: &AuraConfig,
    registry: &LlmProviderRegistry,
    blob: Arc<dyn BlobStore>,
    vault: Arc<SecretVault>,
    cost_manager: &Arc<CostManager>,
) -> anyhow::Result<(HashMap<LlmEntryName, Arc<GuardedLlm>>, Vec<LlmEntryName>)> {
    let results = futures::future::join_all(config.llm.iter().map(|entry| {
        let blob = blob.clone();
        let vault = Arc::clone(&vault);
        let guard = cost_call_guard(cost_manager);
        async move {
            let r =
                boot::build_llm_client_for_entry(entry, registry, Some(blob), Some(vault), guard)
                    .await;
            (entry.name.clone(), r)
        }
    }))
    .await;

    let mut clients = HashMap::new();
    let mut dropped = Vec::new();
    for (name, result) in results {
        match result {
            Ok(client) => {
                clients.insert(name, client);
            }
            Err(e) => {
                if name == config.default_llm {
                    return Err(e);
                }
                warn!(
                    entry = %name,
                    error = %e,
                    "failed to build LLM client for entry; it is unavailable until resolved"
                );
                dropped.push(name);
            }
        }
    }
    Ok((clients, dropped))
}

/// Pricing overlay (`model id → pricing`) harvested from built clients,
/// keyed by `model_info.id` to match the cost lookup in
/// `aura_agent`'s `billed_chat`.
pub(crate) fn pricing_overlay(
    clients: &HashMap<LlmEntryName, Arc<GuardedLlm>>,
) -> HashMap<String, ModelPricing> {
    clients
        .values()
        .map(|c| {
            let info = c.model_info();
            (info.id.clone(), info.pricing)
        })
        .collect()
}

/// `(provider, model)` pairs for the OpenRouter live-pricing refresh.
pub(crate) fn refresh_pairs(config: &AuraConfig) -> Vec<(String, String)> {
    config
        .llm
        .iter()
        .map(|e| (e.provider.clone(), e.model.clone()))
        .collect()
}

/// Spawn the OpenRouter live-pricing refresh loop and return its
/// cancellation token. The task exits on its own token (cancelled by
/// the next reload) or the global shutdown. The first fetch runs before
/// the first sleep so a freshly-(re)configured model gets a live
/// overlay promptly.
pub(crate) fn spawn_pricing_refresh(
    cost_manager: &Arc<CostManager>,
    pairs: Vec<(String, String)>,
    shutdown: &ShutdownSignal,
) -> CancellationToken {
    let token = CancellationToken::new();
    // No configured (provider, model) pairs ⇒ nothing to refresh. Return a
    // detached token (cancelling it is a harmless no-op) instead of
    // spawning a task that fetches an empty overlay and then sleeps until
    // the next reload.
    if pairs.is_empty() {
        return token;
    }
    let cm = Arc::clone(cost_manager);
    let shutdown = shutdown.clone();
    let task_token = token.clone();
    tokio::spawn(async move {
        loop {
            let entries: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(p, m)| (p.as_str(), m.as_str()))
                .collect();
            let overlay = aura_llm::openrouter::fetch_overlay_for(&entries).await;
            let pricings = overlay
                .into_iter()
                .map(|(model, (pricing, _caps))| (model, pricing))
                .collect();
            cm.merge_pricings(pricings);
            tokio::select! {
                _ = tokio::time::sleep(aura_llm::openrouter::REFRESH_INTERVAL) => {}
                _ = task_token.cancelled() => break,
                _ = shutdown.wait() => break,
            }
        }
    });
    token
}

/// LLM-identity consumer: rebuilds the pool, reseeds cost pricing, and
/// restarts the refresh loop. Owns the refresh task's cancellation
/// token from boot so a reload can cancel + respawn it.
pub(crate) struct LlmReloader {
    pool_handle: LlmPoolHandle,
    cost_manager: Arc<CostManager>,
    blob: Arc<dyn BlobStore>,
    vault: Arc<SecretVault>,
    shutdown: ShutdownSignal,
    refresh_cancel: Mutex<CancellationToken>,
}

/// Result of [`LlmReloader::prepare`] — everything `commit` needs, plus
/// the operator-facing outcome.
pub(crate) struct PreparedLlm {
    pool: LlmClientPool,
    overlay: HashMap<String, ModelPricing>,
    pairs: Vec<(String, String)>,
    outcome: ReloadOutcome,
}

impl LlmReloader {
    /// Construct and spawn the initial refresh loop.
    pub(crate) fn new(
        pool_handle: LlmPoolHandle,
        cost_manager: Arc<CostManager>,
        blob: Arc<dyn BlobStore>,
        vault: Arc<SecretVault>,
        shutdown: ShutdownSignal,
        initial_pairs: Vec<(String, String)>,
    ) -> Self {
        let token = spawn_pricing_refresh(&cost_manager, initial_pairs, &shutdown);
        Self {
            pool_handle,
            cost_manager,
            blob,
            vault,
            shutdown,
            refresh_cancel: Mutex::new(token),
        }
    }

    /// Fallible: rebuild the pool from the new config. Aborts the whole
    /// reload if the default entry fails to build.
    async fn prepare(&self, new: &AuraConfig) -> Result<PreparedLlm, String> {
        let registry = LlmProviderRegistry::with_default_providers();
        let (clients, dropped) = build_pool_clients(
            new,
            &registry,
            Arc::clone(&self.blob),
            Arc::clone(&self.vault),
            &self.cost_manager,
        )
        .await
        .map_err(|e| e.to_string())?;
        let overlay = pricing_overlay(&clients);
        let pool = LlmClientPool::with_tier_map(
            clients,
            new.default_llm.clone(),
            new.agent.model_tiers.clone(),
        )?;
        let active_model = pool.default_client().model_info().id.clone();
        let mut entries: Vec<String> = pool
            .entry_names()
            .iter()
            .map(|n| n.as_str().to_string())
            .collect();
        entries.sort();
        let outcome = ReloadOutcome {
            active_model,
            default_entry: new.default_llm.as_str().to_string(),
            entries,
            dropped: dropped.iter().map(|n| n.as_str().to_string()).collect(),
        };
        Ok(PreparedLlm {
            pool,
            overlay,
            pairs: refresh_pairs(new),
            outcome,
        })
    }

    /// Infallible: reseed pricing (before the pool swap, to close the
    /// $0-billing window), swap the pool, and restart the refresh loop.
    fn commit(&self, prepared: PreparedLlm) -> ReloadOutcome {
        self.cost_manager.merge_pricings(prepared.overlay);
        *self.pool_handle.write() = Arc::new(prepared.pool);
        let mut guard = self.refresh_cancel.lock();
        guard.cancel();
        *guard = spawn_pricing_refresh(&self.cost_manager, prepared.pairs, &self.shutdown);
        prepared.outcome
    }
}

/// Cost consumer: swaps the spending caps and the live rate-limit knobs.
/// Both are infallible to prepare (just new values, no rebuild).
pub(crate) struct CostReloader {
    cost_manager: Arc<CostManager>,
    rate_limit: Arc<LiveRateLimit>,
}

struct PreparedCost {
    limits: SpendingLimits,
    max_requests: usize,
    window: Duration,
}

impl CostReloader {
    pub(crate) fn new(cost_manager: Arc<CostManager>, rate_limit: Arc<LiveRateLimit>) -> Self {
        Self {
            cost_manager,
            rate_limit,
        }
    }

    fn prepare(&self, new: &AuraConfig) -> PreparedCost {
        PreparedCost {
            limits: SpendingLimits {
                daily_usd: new.cost.spending_limits.daily_usd,
                monthly_usd: new.cost.spending_limits.monthly_usd,
            },
            max_requests: new.cost.rate_limit.max_requests,
            window: Duration::from_secs(new.cost.rate_limit.window_secs),
        }
    }

    fn commit(&self, p: PreparedCost) {
        self.cost_manager.set_limits(p.limits);
        self.rate_limit.set(p.max_requests, p.window);
    }
}

/// Orchestrator: validate → diff → prepare-all → commit-all → swap
/// config. Serialized by `reload_lock` so concurrent triggers (admin
/// endpoint + SIGHUP) never interleave.
pub struct RuntimeConfigReloader {
    config_path: Option<PathBuf>,
    handle: ConfigHandle,
    reload_lock: tokio::sync::Mutex<()>,
    llm: LlmReloader,
    cost: CostReloader,
}

impl RuntimeConfigReloader {
    pub fn new(
        config_path: Option<PathBuf>,
        handle: ConfigHandle,
        llm: LlmReloader,
        cost: CostReloader,
    ) -> Self {
        Self {
            config_path,
            handle,
            reload_lock: tokio::sync::Mutex::new(()),
            llm,
            cost,
        }
    }
}

#[async_trait]
impl ConfigReloader for RuntimeConfigReloader {
    async fn reload(&self) -> Result<ReloadOutcome, ReloadError> {
        let path = self.config_path.as_ref().ok_or(ReloadError::NoConfigPath)?;
        // Serialize concurrent reloads so prepares/commits never interleave.
        let _guard = self.reload_lock.lock().await;

        let new = Arc::new(
            AuraConfig::load_from_file(path)
                .await
                .map_err(|e| ReloadError::Config(e.to_string()))?,
        );
        let old = self.handle.current();

        if let Err(e) = hot_reload_diff(&old, &new) {
            // A non-hot field on disk differs from the last config we
            // processed. We can't apply it live (it needs a restart), but we
            // must still advance the baseline to this validated on-disk
            // config — otherwise the divergence re-trips on *every* future
            // reload and silently blocks all hot reloads until restart (a
            // persisted non-hot edit via `PUT /v1/config` is a supported
            // path). Nothing reads the handle for live behaviour — it is
            // purely the diff baseline — so storing the persisted (not-yet-
            // applied) config is bookkeeping only; the live pool/cost state
            // is untouched. The effect: `hot_reload_diff` compares each
            // reload against the previous on-disk state, i.e. this edit's
            // delta, not an ever-staler boot baseline. See
            // `docs/config-hot-reload.md`.
            self.handle.store(Arc::clone(&new));
            return Err(ReloadError::NotHotReloadable(e.to_string()));
        }

        // Always rebuild the LLM pool. A config `llm` change and a vault
        // credential rotation (which is invisible in the config diff) both
        // demand a fresh pool, and there's no cheap way to prove
        // "credentials unchanged" — so gating the rebuild on the diff alone
        // would let a key rotation keep serving the old credential. The
        // rebuild is local/cheap and the per-turn `Arc::ptr_eq` rebind is
        // prompt-cache-safe, so an unconditional rebuild is the simple,
        // correct choice. See `docs/config-hot-reload.md`.
        let prepared_llm = self
            .llm
            .prepare(&new)
            .await
            .map_err(ReloadError::LlmRebuild)?;
        let prepared_cost = self.cost.prepare(&new);

        // Commit (infallible): swaps + live setters. Pricing is reseeded
        // before the pool swap inside `llm.commit`.
        let outcome = self.llm.commit(prepared_llm);
        self.cost.commit(prepared_cost);
        self.handle.store(Arc::clone(&new));

        info!(
            active_model = %outcome.active_model,
            default_entry = %outcome.default_entry,
            dropped = ?outcome.dropped,
            "config hot-reload applied",
        );
        Ok(outcome)
    }

    async fn dry_run(&self, candidate: &AuraConfig) -> Result<(), ReloadError> {
        // Build the candidate's pool to confirm it's buildable, then discard
        // it (no swap, no pricing reseed, no refresh spawn). Serialized
        // against real reloads so a concurrent swap can't interleave.
        //
        // The admin endpoints call this and then `reload`, so a mutating
        // request builds the pool twice. Client construction is local (no
        // network), so the redundancy is cheap; folding it out would mean
        // `prepare` returning a handle `reload` consumes — more plumbing
        // than it's worth for an admin-rate operation.
        let _guard = self.reload_lock.lock().await;
        self.llm
            .prepare(candidate)
            .await
            .map_err(ReloadError::LlmRebuild)?;
        Ok(())
    }
}
