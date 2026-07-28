//! In-process config hot-reload orchestrator.
//!
//! Implements [`baybo_gateway::ConfigReloader`]. Lives in the bin crate
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
use baybo_agent::router::LiveRateLimit;
use baybo_agent::service::ShutdownSignal;
use baybo_agent::{LlmClientPool, LlmPoolHandle};
use baybo_config::{BayboConfig, ConfigHandle, hot_reload_diff};
use baybo_cost::{CostManager, SpendingLimits, cost_hooks};
use baybo_gateway::{ConfigReloader, ReloadError, ReloadOutcome};
use baybo_llm::{BillableLlm, LlmProviderRegistry, ModelPricing};
use baybo_model::LlmEntryName;
use baybo_security::SecretVault;
use baybo_store::BlobStore;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::boot;

/// The built LLM clients for a config: one default-model client per entry,
/// plus a client per further `model_list` model, plus the per-entry pinnable
/// model list and lite client — everything
/// [`baybo_agent::LlmPoolConfig`] needs. `dropped` names entries whose
/// DEFAULT client failed to build.
pub(crate) struct BuiltPoolClients {
    pub clients: HashMap<LlmEntryName, Arc<BillableLlm>>,
    pub overrides: HashMap<(LlmEntryName, String), Arc<BillableLlm>>,
    pub entry_models: HashMap<LlmEntryName, Vec<String>>,
    pub lite: HashMap<LlmEntryName, Arc<BillableLlm>>,
    pub dropped: Vec<LlmEntryName>,
}

/// Build every LLM client a config implies, concurrently: one per
/// `(entry, model)` across each entry's `models()`. Failure policy: a
/// **default** entry whose default model fails to build is a hard error (the
/// pool would be unusable); any other build failure (a non-default entry, or
/// any non-default model) is dropped with a `warn!`. Only models that
/// actually built a client are pinnable, so `entry_models` lists just those.
pub(crate) async fn build_pool_clients(
    config: &BayboConfig,
    registry: &LlmProviderRegistry,
    blob: Arc<dyn BlobStore>,
    vault: Arc<SecretVault>,
    cost_manager: &Arc<CostManager>,
) -> anyhow::Result<BuiltPoolClients> {
    let proxy = boot::proxy_settings(config);

    // One build job per (entry, model) across the entry's normalized
    // `models()`. `is_default` marks the entry's primary model (goes into
    // `clients`); the rest go to `overrides`. A model listed twice builds
    // once.
    struct Job<'a> {
        entry: &'a baybo_config::LlmEntry,
        spec: baybo_config::LlmModelSpec,
        is_default: bool,
    }
    let jobs: Vec<Job<'_>> = config
        .llm
        .iter()
        .flat_map(|entry| {
            let mut seen = std::collections::HashSet::new();
            entry
                .models()
                .into_iter()
                .filter(move |spec| seen.insert(spec.model.clone()))
                .map(move |spec| Job {
                    entry,
                    is_default: spec.model == entry.model,
                    spec,
                })
        })
        .collect();

    let results = futures::future::join_all(jobs.into_iter().map(|job| {
        let blob = blob.clone();
        let vault = Arc::clone(&vault);
        let billing = cost_hooks(cost_manager);
        let proxy = proxy.clone();
        async move {
            let r = boot::build_llm_client_for_entry_model(
                job.entry,
                &job.spec,
                registry,
                Some(blob),
                Some(vault),
                billing,
                proxy,
            )
            .await;
            (job.entry.name.clone(), job.spec.model, job.is_default, r)
        }
    }))
    .await;

    let mut clients = HashMap::new();
    let mut overrides = HashMap::new();
    let mut candidate_ok: Vec<(LlmEntryName, String)> = Vec::new();
    let mut dropped = Vec::new();
    for (name, model, is_default, result) in results {
        match (result, is_default) {
            (Ok(client), true) => {
                clients.insert(name, client);
            }
            (Ok(client), false) => {
                overrides.insert((name.clone(), model.clone()), client);
                candidate_ok.push((name, model));
            }
            (Err(e), true) => {
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
            (Err(e), false) => {
                warn!(
                    entry = %name,
                    model = %model,
                    error = %e,
                    "failed to build LLM client for listed model; it is unpickable until resolved"
                );
            }
        }
    }

    // A non-default model is only pinnable if its entry's default client
    // also built. `entry_models[name]` follows `models()` order, which is
    // the chat picker's order.
    let mut entry_models: HashMap<LlmEntryName, Vec<String>> = HashMap::new();
    for entry in &config.llm {
        if clients.contains_key(&entry.name) {
            let mut seen = std::collections::HashSet::new();
            let models: Vec<String> = entry
                .models()
                .into_iter()
                .map(|spec| spec.model)
                .filter(|model| seen.insert(model.clone()))
                .filter(|model| {
                    model == &entry.model
                        || candidate_ok
                            .iter()
                            .any(|(n, m)| n == &entry.name && m == model)
                })
                .collect();
            entry_models.insert(entry.name.clone(), models);
        } else {
            // Entry's default failed → drop any candidates that built for it.
            overrides.retain(|(n, _), _| n != &entry.name);
        }
    }

    // `validate()` guarantees `lite_model` names one of the entry's own
    // models, so the client already exists — this is a lookup, never a
    // second build.
    //
    // A surviving entry that declares a lite model and hasn't got one is a
    // HARD error, unlike a merely-unpickable `model_list` model. Strictness
    // tracks observability, not importance: a dropped pickable model is
    // visible the moment a user opens the picker, whereas a dropped lite
    // has no symptom at all beyond the bill quietly staying at main-model
    // rates. Client construction is local and offline, so this can only
    // fire on a deterministic config error, never on provider flap.
    let mut lite = HashMap::new();
    for entry in &config.llm {
        let Some(model) = entry.lite_model.as_deref() else {
            continue;
        };
        // An entry whose own default failed is already dropped; chasing its
        // lite would just be a secondary failure of a thing nobody can use.
        let Some(default_client) = clients.get(&entry.name) else {
            continue;
        };
        let client = if model == entry.model {
            Some(default_client.clone())
        } else {
            overrides
                .get(&(entry.name.clone(), model.to_string()))
                .cloned()
        };
        match client {
            Some(client) => {
                lite.insert(entry.name.clone(), client);
            }
            None => {
                return Err(anyhow::anyhow!(
                    "llm entry {:?} declares lite_model {model:?} but no client for it was built; \
                     it must name one of the entry's model_list models and that model must build",
                    entry.name.as_str(),
                ));
            }
        }
    }

    Ok(BuiltPoolClients {
        clients,
        overrides,
        entry_models,
        lite,
        dropped,
    })
}

/// Pricing overlay (`model id → pricing`) harvested from every built client
/// — default AND candidate — keyed by `model_info.id` to match the cost
/// lookup in `baybo_agent`'s `billed_chat`, so a pinned candidate model bills
/// correctly too.
pub(crate) fn pricing_overlay(built: &BuiltPoolClients) -> HashMap<String, ModelPricing> {
    built
        .clients
        .values()
        .chain(built.overrides.values())
        .map(|c| {
            let info = c.model_info();
            (info.id.clone(), info.pricing)
        })
        .collect()
}

/// `(provider, model)` pairs for the OpenRouter live-pricing refresh —
/// every model of every entry, not just each entry's default. A model the
/// refresh loop never asks about keeps its boot-time snapshot price
/// forever, which used to silently apply to every non-default model.
pub(crate) fn refresh_pairs(config: &BayboConfig) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    config
        .llm
        .iter()
        .flat_map(|e| {
            e.models()
                .into_iter()
                .map(move |spec| (e.provider.clone(), spec.model))
        })
        .filter(|pair| seen.insert(pair.clone()))
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
    proxy: Option<baybo_security::http::ProxySettings>,
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
            let overlay = baybo_llm::openrouter::fetch_overlay_for(&entries, proxy.as_ref()).await;
            let pricings = overlay
                .into_iter()
                .map(|(model, (pricing, _caps))| (model, pricing))
                .collect();
            cm.merge_pricings(pricings);
            tokio::select! {
                _ = tokio::time::sleep(baybo_llm::openrouter::REFRESH_INTERVAL) => {}
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
    /// Egress proxy for the pricing-refresh loop. Fixed at boot — `proxy`
    /// is not hot-reloadable (a change rejects the reload), so the reloader
    /// keeps the boot value across pool swaps.
    proxy: Option<baybo_security::http::ProxySettings>,
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
        proxy: Option<baybo_security::http::ProxySettings>,
    ) -> Self {
        let token = spawn_pricing_refresh(&cost_manager, initial_pairs, proxy.clone(), &shutdown);
        Self {
            pool_handle,
            cost_manager,
            blob,
            vault,
            shutdown,
            proxy,
            refresh_cancel: Mutex::new(token),
        }
    }

    /// Fallible: rebuild the pool from the new config. Aborts the whole
    /// reload if the default entry fails to build.
    async fn prepare(&self, new: &BayboConfig) -> Result<PreparedLlm, String> {
        let registry = LlmProviderRegistry::with_default_providers();
        let built = build_pool_clients(
            new,
            &registry,
            Arc::clone(&self.blob),
            Arc::clone(&self.vault),
            &self.cost_manager,
        )
        .await
        .map_err(|e| e.to_string())?;
        let overlay = pricing_overlay(&built);
        let dropped = built.dropped.clone();
        let pool = LlmClientPool::from_config(baybo_agent::LlmPoolConfig {
            clients: built.clients,
            overrides: built.overrides,
            entry_models: built.entry_models,
            lite: built.lite,
            default_name: new.default_llm.clone(),
            tier_map: new.agent.model_tiers.clone(),
        })?;
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
        *guard = spawn_pricing_refresh(
            &self.cost_manager,
            prepared.pairs,
            self.proxy.clone(),
            &self.shutdown,
        );
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

    fn prepare(&self, new: &BayboConfig) -> PreparedCost {
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
    /// Shared Bash permission mode; a hot reload swaps it (see `commit` below) so
    /// running `BashTool`s pick up the new isolation/approval behavior and
    /// description live.
    bash_permission: Arc<baybo_tools::builtin::LivePermissionMode>,
}

impl RuntimeConfigReloader {
    pub fn new(
        config_path: Option<PathBuf>,
        handle: ConfigHandle,
        llm: LlmReloader,
        cost: CostReloader,
        bash_permission: Arc<baybo_tools::builtin::LivePermissionMode>,
    ) -> Self {
        Self {
            config_path,
            handle,
            reload_lock: tokio::sync::Mutex::new(()),
            llm,
            cost,
            bash_permission,
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
            BayboConfig::load_from_file(path)
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
        // Swap the Bash permission mode live: the next command (and the next tool
        // description the LLM sees) observes the new isolation/approval policy.
        self.bash_permission
            .set(crate::boot::to_bash_permission(new.permission));
        self.handle.store(Arc::clone(&new));

        info!(
            active_model = %outcome.active_model,
            default_entry = %outcome.default_entry,
            dropped = ?outcome.dropped,
            "config hot-reload applied",
        );
        Ok(outcome)
    }

    async fn dry_run(&self, candidate: &BayboConfig) -> Result<(), ReloadError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_config::{LlmEntry, LlmModelSpec};

    fn entry(name: &str, provider: &str, model: &str, list: Vec<LlmModelSpec>) -> LlmEntry {
        LlmEntry {
            name: LlmEntryName::from(name),
            provider: provider.into(),
            model: model.into(),
            model_list: list,
            lite_model: None,
            api_key_env: None,
            base_url: None,
            reasoning_effort: None,
        }
    }

    fn config_with(entries: Vec<LlmEntry>) -> BayboConfig {
        let default = entries[0].name.clone();
        BayboConfig {
            llm: entries,
            default_llm: default,
            ..Default::default()
        }
    }

    /// Every model gets live pricing, not just each entry's default —
    /// a model the refresh loop never asks about keeps its boot-time
    /// snapshot price forever.
    #[test]
    fn refresh_pairs_covers_every_model_of_every_entry() {
        let cfg = config_with(vec![
            entry(
                "primary",
                "openai",
                "gpt-5",
                vec![LlmModelSpec::bare("gpt-5-mini")],
            ),
            entry("alt", "anthropic", "claude-opus-4", vec![]),
        ]);
        let pairs = refresh_pairs(&cfg);
        assert_eq!(
            pairs,
            vec![
                ("openai".to_string(), "gpt-5".to_string()),
                ("openai".to_string(), "gpt-5-mini".to_string()),
                ("anthropic".to_string(), "claude-opus-4".to_string()),
            ]
        );
    }

    /// Two entries on the same provider+model must not queue the same
    /// OpenRouter lookup twice.
    #[test]
    fn refresh_pairs_dedupes_across_entries() {
        let cfg = config_with(vec![
            entry("a", "openai", "gpt-5", vec![]),
            entry("b", "openai", "gpt-5", vec![]),
        ]);
        assert_eq!(refresh_pairs(&cfg).len(), 1);
    }
}
