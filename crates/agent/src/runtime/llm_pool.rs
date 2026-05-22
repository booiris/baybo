//! Process-wide pool of guarded LLM clients keyed by config entry name.
//!
//! Built once at startup from `AuraConfig.llm` and held on `ManagerGraph`
//! so per-actor consumers can resolve a specific entry by name at turn
//! dispatch time. `resolve(None)` returns the default entry;
//! `resolve(Some(name))` returns that entry if present, otherwise the
//! default with a `warn!` (stranded reference).
//!
//! Optional `model_tiers` (Fast / Balanced / Deep → entry name) lets
//! `spawn_subagent` ask for "a fast model" without knowing which
//! `aura.json` entry happens to be wired to that tier. Unmapped tiers
//! return `None` and the caller falls through to the pool default.

use std::collections::HashMap;
use std::sync::Arc;

use aura_llm::GuardedLlm;
use aura_model::{LlmEntryName, ModelTier};
use tracing::warn;

pub struct LlmClientPool {
    clients: HashMap<LlmEntryName, Arc<GuardedLlm>>,
    default_name: LlmEntryName,
    default_client: Arc<GuardedLlm>,
    tier_map: HashMap<ModelTier, LlmEntryName>,
}

impl LlmClientPool {
    pub fn new(
        clients: HashMap<LlmEntryName, Arc<GuardedLlm>>,
        default_name: LlmEntryName,
    ) -> Result<Self, String> {
        Self::with_tier_map(clients, default_name, HashMap::new())
    }

    /// Construct a pool with a tier→entry-name lookup. Each value in
    /// `tier_map` must already exist in `clients` — a stranded
    /// reference would surface as a default-fallback every spawn,
    /// which is hard to diagnose at runtime, so we reject it at boot.
    pub fn with_tier_map(
        clients: HashMap<LlmEntryName, Arc<GuardedLlm>>,
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

    pub fn default_client(&self) -> Arc<GuardedLlm> {
        self.default_client.clone()
    }

    pub fn entry_names(&self) -> Vec<LlmEntryName> {
        self.clients.keys().cloned().collect()
    }

    pub(crate) fn resolve(&self, name: Option<&LlmEntryName>) -> (Arc<GuardedLlm>, LlmEntryName) {
        match name {
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
        }
    }
}

fn entry_names_csv(clients: &HashMap<LlmEntryName, Arc<GuardedLlm>>) -> String {
    clients
        .keys()
        .map(LlmEntryName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_llm::test_support::StubLlm;
    use aura_llm::{LlmCompletion, ModelInfo, ModelPricing};
    use std::sync::Arc;

    fn stub_with_id(id: &str) -> Arc<GuardedLlm> {
        let info = ModelInfo {
            id: id.to_string(),
            provider: "stub".into(),
            context_window: 100_000,
            supports_tools: false,
            supports_vision: false,
            pricing: ModelPricing::default(),
        };
        let stub = Arc::new(StubLlm::new().with_model_info(info));
        GuardedLlm::passthrough(stub as Arc<dyn LlmCompletion>)
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

    #[test]
    fn resolve_none_returns_default() {
        let pool = fixture();
        let (client, name) = pool.resolve(None);
        assert_eq!(name, "primary");
        assert_eq!(client.model_info().id, "model-primary");
    }

    #[test]
    fn resolve_known_returns_match() {
        let pool = fixture();
        let (client, name) = pool.resolve(Some(&LlmEntryName::from("fast")));
        assert_eq!(name, "fast");
        assert_eq!(client.model_info().id, "model-fast");
    }

    #[test]
    fn resolve_unknown_falls_back_to_default() {
        let pool = fixture();
        let (client, name) = pool.resolve(Some(&LlmEntryName::from("ghost")));
        assert_eq!(name, "primary");
        assert_eq!(client.model_info().id, "model-primary");
    }

    #[test]
    fn default_client_matches_resolve_none() {
        let pool = fixture();
        let direct = pool.default_client();
        let (resolved, _) = pool.resolve(None);
        assert!(Arc::ptr_eq(&direct, &resolved));
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
