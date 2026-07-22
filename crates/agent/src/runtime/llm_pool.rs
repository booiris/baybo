//! Process-wide pool of guarded LLM clients keyed by config entry name.
//!
//! Built once at startup from `BayboConfig.llm` and held on `ManagerGraph`
//! so per-actor consumers can resolve a specific entry by name at turn
//! dispatch time. `resolve(None)` returns the default entry;
//! `resolve(Some(name))` returns that entry if present, otherwise the
//! default with a `warn!` (stranded reference).
//!
//! Optional `model_tiers` (Fast / Balanced / Deep → entry name) lets
//! `spawn_subagent` ask for "a fast model" without knowing which
//! `baybo.json` entry happens to be wired to that tier. Unmapped tiers
//! return `None` and the caller falls through to the pool default.

use std::collections::HashMap;
use std::sync::Arc;

use baybo_llm::BillableLlm;
use baybo_model::{LlmEntryName, ModelTier};
use tracing::warn;

/// Process-wide, hot-swappable handle to the [`LlmClientPool`]. A config
/// reload swaps the inner `Arc<LlmClientPool>` under the write lock;
/// readers (each `AgentLoop` at turn start, the router's tier
/// resolution, the gateway's `GET /v1/llm`) clone the current `Arc` and
/// use it for that turn/request. A plain `RwLock<Arc<_>>` rather than a
/// lock-free `ArcSwap` because reads happen per-turn / per-request,
/// never per-token. See `docs/config-hot-reload.md`.
pub type LlmPoolHandle = Arc<parking_lot::RwLock<Arc<LlmClientPool>>>;

pub struct LlmClientPool {
    /// Entry → its DEFAULT-model client.
    clients: HashMap<LlmEntryName, Arc<BillableLlm>>,
    /// (entry, candidate model id) → client, for models other than the
    /// entry's default. Pre-built at boot/reload from `model_candidates`.
    overrides: HashMap<(LlmEntryName, String), Arc<BillableLlm>>,
    /// Entry → every model id it can serve (`[default] + candidates` that
    /// actually built a client). The pinnable set for validation.
    entry_models: HashMap<LlmEntryName, Vec<String>>,
    default_name: LlmEntryName,
    default_client: Arc<BillableLlm>,
    tier_map: HashMap<ModelTier, LlmEntryName>,
}

impl LlmClientPool {
    pub fn new(
        clients: HashMap<LlmEntryName, Arc<BillableLlm>>,
        default_name: LlmEntryName,
    ) -> Result<Self, String> {
        Self::with_tier_map(clients, default_name, HashMap::new())
    }

    /// Construct a pool with a tier→entry-name lookup but NO candidate
    /// models — each entry serves only its default model. `entry_models` is
    /// derived from each client's own `model_info().id`.
    pub fn with_tier_map(
        clients: HashMap<LlmEntryName, Arc<BillableLlm>>,
        default_name: LlmEntryName,
        tier_map: HashMap<ModelTier, LlmEntryName>,
    ) -> Result<Self, String> {
        let entry_models = clients
            .iter()
            .map(|(name, client)| (name.clone(), vec![client.model_info().id.clone()]))
            .collect();
        Self::with_candidates(
            clients,
            HashMap::new(),
            entry_models,
            default_name,
            tier_map,
        )
    }

    /// The full constructor: default-model clients plus candidate-model
    /// `overrides` and the per-entry pinnable-model list. Each value in
    /// `tier_map` must already exist in `clients` — a stranded reference
    /// would surface as a default-fallback every spawn, which is hard to
    /// diagnose at runtime, so we reject it at boot.
    pub fn with_candidates(
        clients: HashMap<LlmEntryName, Arc<BillableLlm>>,
        overrides: HashMap<(LlmEntryName, String), Arc<BillableLlm>>,
        entry_models: HashMap<LlmEntryName, Vec<String>>,
        default_name: LlmEntryName,
        tier_map: HashMap<ModelTier, LlmEntryName>,
    ) -> Result<Self, String> {
        let default_client = clients.get(&default_name).cloned().ok_or_else(|| {
            format!(
                "default-llm {default_name:?} not present in client pool; configured entries: [{}]",
                entry_names_csv(&clients)
            )
        })?;
        for (tier, target) in &tier_map {
            if !clients.contains_key(target) {
                return Err(format!(
                    "model_tiers[{tier}] points at unknown llm entry {target:?}; configured entries: [{}]",
                    entry_names_csv(&clients)
                ));
            }
        }
        Ok(Self {
            clients,
            overrides,
            entry_models,
            default_name,
            default_client,
            tier_map,
        })
    }

    /// Look up the entry name bound to a `ModelTier`. Returns `None`
    /// when the tier is unmapped — callers (typically the subagent
    /// router) treat that as "fall back to pool default".
    pub fn resolve_tier(&self, tier: ModelTier) -> Option<LlmEntryName> {
        self.tier_map.get(&tier).cloned()
    }

    pub fn default_client(&self) -> Arc<BillableLlm> {
        self.default_client.clone()
    }

    pub fn entry_names(&self) -> Vec<LlmEntryName> {
        self.clients.keys().cloned().collect()
    }

    /// The model ids entry `name` can be pinned to (`[default] + candidates`
    /// that built a client), or `None` if the entry isn't in the pool. Used
    /// by the gateway to validate a `(entry, model)` session pin.
    pub fn entry_model_ids(&self, name: &LlmEntryName) -> Option<&[String]> {
        self.entry_models.get(name).map(Vec::as_slice)
    }

    /// Resolve a session's pin to a client. `name` picks the entry (`None` =
    /// default-llm, stranded name → default with a warn). `model` picks the
    /// model WITHIN that entry: `None` or the entry's default model returns
    /// the entry's default client; a configured candidate returns its
    /// pre-built client; a stranded model degrades to the entry default with
    /// a warn. The returned name is always the ENTRY name (the pin's
    /// identity), never the model.
    pub(crate) fn resolve(
        &self,
        name: Option<&LlmEntryName>,
        model: Option<&str>,
    ) -> (Arc<BillableLlm>, LlmEntryName) {
        let (base, entry_name) = match name {
            None => (self.default_client(), self.default_name.clone()),
            Some(requested) => match self.clients.get(requested) {
                Some(client) => (client.clone(), requested.clone()),
                None => {
                    warn!(
                        requested = %requested,
                        default = %self.default_name,
                        "llm entry not found in pool, falling back to default-llm"
                    );
                    (self.default_client(), self.default_name.clone())
                }
            },
        };
        let Some(model) = model else {
            return (base, entry_name);
        };
        // The entry's default model resolves to the base client already.
        if base.model_info().id == model {
            return (base, entry_name);
        }
        match self.overrides.get(&(entry_name.clone(), model.to_string())) {
            Some(client) => (client.clone(), entry_name),
            None => {
                warn!(
                    entry = %entry_name,
                    model = %model,
                    "pinned model is not a configured candidate of the entry, falling back to the entry's default model"
                );
                (base, entry_name)
            }
        }
    }
}

fn entry_names_csv(clients: &HashMap<LlmEntryName, Arc<BillableLlm>>) -> String {
    clients
        .keys()
        .map(LlmEntryName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_llm::test_support::StubLlm;
    use baybo_llm::{LlmCompletion, ModelInfo, ModelPricing};
    use std::sync::Arc;

    fn stub_with_id(id: &str) -> Arc<BillableLlm> {
        let info = ModelInfo {
            id: id.to_string(),
            provider: "stub".into(),
            context_window: 100_000,
            supports_tools: false,
            supports_vision: false,
            pricing: ModelPricing::default(),
        };
        let stub = Arc::new(StubLlm::new().with_model_info(info));
        BillableLlm::passthrough(stub as Arc<dyn LlmCompletion>)
    }

    fn fixture() -> LlmClientPool {
        let mut clients = HashMap::new();
        clients.insert(LlmEntryName::from("primary"), stub_with_id("model-primary"));
        clients.insert(LlmEntryName::from("fast"), stub_with_id("model-fast"));
        LlmClientPool::new(clients, LlmEntryName::from("primary")).unwrap()
    }

    #[test]
    fn new_rejects_missing_default() {
        let mut clients = HashMap::new();
        clients.insert(LlmEntryName::from("a"), stub_with_id("a"));
        match LlmClientPool::new(clients, LlmEntryName::from("missing")) {
            Ok(_) => panic!("expected error"),
            Err(msg) => assert!(msg.contains("missing"), "unexpected error message: {msg}"),
        }
    }

    /// A pool with one entry `primary` (default model `model-primary`) that
    /// also offers a candidate model `model-alt`.
    fn candidate_fixture() -> LlmClientPool {
        let mut clients = HashMap::new();
        clients.insert(LlmEntryName::from("primary"), stub_with_id("model-primary"));
        let mut overrides = HashMap::new();
        overrides.insert(
            (LlmEntryName::from("primary"), "model-alt".to_string()),
            stub_with_id("model-alt"),
        );
        let mut entry_models = HashMap::new();
        entry_models.insert(
            LlmEntryName::from("primary"),
            vec!["model-primary".to_string(), "model-alt".to_string()],
        );
        LlmClientPool::with_candidates(
            clients,
            overrides,
            entry_models,
            LlmEntryName::from("primary"),
            HashMap::new(),
        )
        .unwrap()
    }

    #[test]
    fn resolve_none_returns_default() {
        let pool = fixture();
        let (client, name) = pool.resolve(None, None);
        assert_eq!(name, "primary");
        assert_eq!(client.model_info().id, "model-primary");
    }

    #[test]
    fn resolve_known_returns_match() {
        let pool = fixture();
        let (client, name) = pool.resolve(Some(&LlmEntryName::from("fast")), None);
        assert_eq!(name, "fast");
        assert_eq!(client.model_info().id, "model-fast");
    }

    #[test]
    fn resolve_unknown_falls_back_to_default() {
        let pool = fixture();
        let (client, name) = pool.resolve(Some(&LlmEntryName::from("ghost")), None);
        assert_eq!(name, "primary");
        assert_eq!(client.model_info().id, "model-primary");
    }

    #[test]
    fn resolve_candidate_model_returns_override_client() {
        let pool = candidate_fixture();
        let (client, name) = pool.resolve(Some(&LlmEntryName::from("primary")), Some("model-alt"));
        assert_eq!(name, "primary", "the pin identity stays the entry name");
        assert_eq!(client.model_info().id, "model-alt");
    }

    #[test]
    fn resolve_default_model_by_name_returns_base_client() {
        let pool = candidate_fixture();
        // Explicitly asking for the entry's default model is the base client,
        // not an override lookup.
        let (client, _) = pool.resolve(Some(&LlmEntryName::from("primary")), Some("model-primary"));
        assert_eq!(client.model_info().id, "model-primary");
    }

    #[test]
    fn resolve_stranded_model_falls_back_to_entry_default() {
        let pool = candidate_fixture();
        let (client, name) =
            pool.resolve(Some(&LlmEntryName::from("primary")), Some("ghost-model"));
        assert_eq!(name, "primary");
        assert_eq!(client.model_info().id, "model-primary");
    }

    #[test]
    fn entry_model_ids_lists_default_plus_candidates() {
        let pool = candidate_fixture();
        let ids = pool
            .entry_model_ids(&LlmEntryName::from("primary"))
            .unwrap();
        assert_eq!(ids, ["model-primary", "model-alt"]);
        assert!(pool.entry_model_ids(&LlmEntryName::from("ghost")).is_none());
    }

    #[test]
    fn default_client_matches_resolve_none() {
        let pool = fixture();
        let direct = pool.default_client();
        let (resolved, _) = pool.resolve(None, None);
        assert!(Arc::ptr_eq(&direct, &resolved));
    }

    #[test]
    fn resolve_is_arc_stable_across_calls() {
        // The per-turn reload check (`AgentLoop::refresh_active_llm`)
        // relies on `resolve` returning the same `Arc` when the pool is
        // unchanged, so an unchanged pool never triggers a needless
        // rebind. A pool swap produces fresh `Arc`s, which is what makes
        // the pointer-identity check fire.
        let pool = fixture();
        let (a, _) = pool.resolve(None, None);
        let (b, _) = pool.resolve(None, None);
        assert!(Arc::ptr_eq(&a, &b));
        let (c, _) = pool.resolve(Some(&LlmEntryName::from("fast")), None);
        let (d, _) = pool.resolve(Some(&LlmEntryName::from("fast")), None);
        assert!(Arc::ptr_eq(&c, &d));
    }

    #[test]
    fn resolve_tier_returns_mapped_entry() {
        let mut clients = HashMap::new();
        clients.insert(LlmEntryName::from("primary"), stub_with_id("model-primary"));
        clients.insert(LlmEntryName::from("fast"), stub_with_id("model-fast"));
        let mut tiers = HashMap::new();
        tiers.insert(ModelTier::Fast, LlmEntryName::from("fast"));
        let pool =
            LlmClientPool::with_tier_map(clients, LlmEntryName::from("primary"), tiers).unwrap();
        assert_eq!(
            pool.resolve_tier(ModelTier::Fast),
            Some(LlmEntryName::from("fast"))
        );
        assert!(pool.resolve_tier(ModelTier::Deep).is_none());
    }

    #[test]
    fn with_tier_map_rejects_stranded_reference() {
        let mut clients = HashMap::new();
        clients.insert(LlmEntryName::from("primary"), stub_with_id("primary"));
        let mut tiers = HashMap::new();
        tiers.insert(ModelTier::Deep, LlmEntryName::from("missing"));
        let err = match LlmClientPool::with_tier_map(clients, LlmEntryName::from("primary"), tiers)
        {
            Ok(_) => panic!("stranded tier reference must be rejected at boot"),
            Err(msg) => msg,
        };
        assert!(err.contains("missing"));
    }
}
