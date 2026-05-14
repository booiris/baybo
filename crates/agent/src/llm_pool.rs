//! Process-wide pool of guarded LLM clients keyed by config entry name.
//!
//! Built once at startup from `AuraConfig.llm` and held on `ManagerGraph`
//! so per-actor consumers can resolve a specific entry by name at turn
//! dispatch time. `resolve(None)` returns the default entry;
//! `resolve(Some(name))` returns that entry if present, otherwise the
//! default with a `warn!` (stranded reference).

use std::collections::HashMap;
use std::sync::Arc;

use aura_llm::GuardedLlm;
use tracing::warn;

pub struct LlmClientPool {
    clients: HashMap<String, Arc<GuardedLlm>>,
    default_name: String,
    default_client: Arc<GuardedLlm>,
}

impl LlmClientPool {
    pub fn new(
        clients: HashMap<String, Arc<GuardedLlm>>,
        default_name: String,
    ) -> Result<Self, String> {
        let default_client = clients.get(&default_name).cloned().ok_or_else(|| {
            format!(
                "default-llm {default_name:?} not present in client pool; configured entries: [{}]",
                clients.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        Ok(Self {
            clients,
            default_name,
            default_client,
        })
    }

    pub fn default_client(&self) -> Arc<GuardedLlm> {
        self.default_client.clone()
    }

    pub fn get(&self, name: &str) -> Option<Arc<GuardedLlm>> {
        self.clients.get(name).cloned()
    }

    pub fn entry_names(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    /// Resolve to a concrete client + the effective entry name (useful
    /// for logging / cost-attribution sanity checks).
    pub(crate) fn resolve(&self, name: Option<&str>) -> (Arc<GuardedLlm>, String) {
        match name {
            None => (self.default_client(), self.default_name.clone()),
            Some(requested) => match self.clients.get(requested) {
                Some(client) => (client.clone(), requested.to_string()),
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
        clients.insert("primary".to_string(), stub_with_id("model-primary"));
        clients.insert("fast".to_string(), stub_with_id("model-fast"));
        LlmClientPool::new(clients, "primary".to_string()).unwrap()
    }

    #[test]
    fn new_rejects_missing_default() {
        let mut clients = HashMap::new();
        clients.insert("a".to_string(), stub_with_id("a"));
        match LlmClientPool::new(clients, "missing".to_string()) {
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
        let (client, name) = pool.resolve(Some("fast"));
        assert_eq!(name, "fast");
        assert_eq!(client.model_info().id, "model-fast");
    }

    #[test]
    fn resolve_unknown_falls_back_to_default() {
        let pool = fixture();
        let (client, name) = pool.resolve(Some("ghost"));
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
}
