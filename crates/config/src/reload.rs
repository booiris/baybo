//! Hot-reload machinery for [`BayboConfig`].
//!
//! `baybo-config` is a leaf crate, so it owns only the pure primitives:
//! a live handle to the applied config ([`ConfigHandle`]) and the
//! whitelist gate ([`hot_reload_diff`]). The fallible derived-state
//! rebuilds (LLM pool, cost limits) and the reload orchestration live
//! in the consumer crates — see `docs/config-hot-reload.md`.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::error::{ConfigError, Result};
use crate::{AgentConfig, BayboConfig, CostConfig};

/// Live, swappable handle to the currently-applied config.
///
/// Reads happen per-turn / per-request (resolving the active model,
/// dashboard reads), never per-token, so a plain `RwLock<Arc<_>>` is
/// ample — a lock-free `ArcSwap` would only add a dependency without a
/// measurable win. The previous `Arc` stays alive until its last
/// in-flight reader drops it, which is what gives the contract's
/// "in-flight requests finish on the old config" behaviour.
#[derive(Clone)]
pub struct ConfigHandle(Arc<RwLock<Arc<BayboConfig>>>);

impl ConfigHandle {
    pub fn new(config: Arc<BayboConfig>) -> Self {
        Self(Arc::new(RwLock::new(config)))
    }

    /// Clone out the currently-applied config.
    pub fn current(&self) -> Arc<BayboConfig> {
        self.0.read().clone()
    }

    /// Swap in a new applied config. Callers must have already passed
    /// [`hot_reload_diff`] + every consumer's prepare step; this is the
    /// infallible commit half.
    pub fn store(&self, config: Arc<BayboConfig>) {
        *self.0.write() = config;
    }
}

/// Reject a reload whose diff touches any field outside the
/// hot-updatable whitelist.
///
/// Whitelist (may change live): `llm`, `default_llm`, `cost`
/// (`rate_limit` + `spending_limits`), and `agent.model_tiers`.
/// Everything else must be byte-identical or the **entire** reload is
/// rejected (atomic — no partial application), naming the first
/// offending section so the operator knows a restart is required.
///
/// `new` is destructured so that adding a field to `BayboConfig` (or
/// `AgentConfig`) forces a hot/non-hot classification here rather than
/// silently defaulting a fresh field to "hot, unchecked".
pub fn hot_reload_diff(old: &BayboConfig, new: &BayboConfig) -> Result<()> {
    let BayboConfig {
        // Hot — free to change.
        llm: _,
        default_llm: _,
        cost: CostConfig {
            spending_limits: _,
            rate_limit: _,
        },
        // Partially hot — only `model_tiers`; handled below.
        agent,
        // Non-hot — any change rejects the reload.
        channels,
        security,
        skills,
        workspace,
        gateway,
        browser,
        external_agents,
        proxy,
        memory,
        // Hot: the runtime reloader swaps the live `BashTool` permission policy
        // (and the tool description it advertises). Ignored here so a
        // `permission`-only change is hot-reloadable rather than rejected.
        permission: _,
    } = new;

    if &old.channels != channels {
        return Err(not_hot("channels"));
    }
    if &old.security != security {
        return Err(not_hot("security"));
    }
    if &old.skills != skills {
        return Err(not_hot("skills"));
    }
    if &old.workspace != workspace {
        return Err(not_hot("workspace"));
    }
    if &old.gateway != gateway {
        return Err(not_hot("gateway"));
    }
    if &old.browser != browser {
        return Err(not_hot("browser"));
    }
    if &old.external_agents != external_agents {
        return Err(not_hot("external_agents"));
    }
    // Changing the proxy means re-creating every HTTP client (LLM, tools,
    // MCP) and re-spawning sidecars with new env — not safe live.
    if &old.proxy != proxy {
        return Err(not_hot("proxy"));
    }
    if &old.memory != memory {
        return Err(not_hot("memory"));
    }

    let AgentConfig {
        max_iterations,
        context,
        max_subagent_depth,
        max_subagents_per_root,
        // Hot.
        model_tiers: _,
    } = agent;
    if &old.agent.max_iterations != max_iterations {
        return Err(not_hot("agent.max_iterations"));
    }
    if &old.agent.context != context {
        return Err(not_hot("agent.context"));
    }
    if &old.agent.max_subagent_depth != max_subagent_depth {
        return Err(not_hot("agent.max_subagent_depth"));
    }
    if &old.agent.max_subagents_per_root != max_subagents_per_root {
        return Err(not_hot("agent.max_subagents_per_root"));
    }

    Ok(())
}

fn not_hot(section: &str) -> ConfigError {
    ConfigError::NotHotReloadable {
        section: section.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmEntry;
    use baybo_model::{LlmEntryName, ModelTier};

    fn base() -> BayboConfig {
        BayboConfig {
            llm: vec![LlmEntry {
                name: LlmEntryName::from("primary"),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                api_key_env: None,
                base_url: None,
                supports_vision: None,
                context_window: None,
                pricing: None,
                reasoning_effort: None,
            }],
            default_llm: LlmEntryName::from("primary"),
            ..Default::default()
        }
    }

    #[test]
    fn identical_is_ok() {
        let c = base();
        assert!(hot_reload_diff(&c, &c.clone()).is_ok());
    }

    #[test]
    fn llm_change_is_hot() {
        let old = base();
        let mut new = base();
        new.llm[0].model = "gpt-4o-mini".into();
        assert!(hot_reload_diff(&old, &new).is_ok());
    }

    #[test]
    fn default_llm_change_is_hot() {
        let old = base();
        let mut new = base();
        new.default_llm = LlmEntryName::from("other");
        assert!(hot_reload_diff(&old, &new).is_ok());
    }

    #[test]
    fn cost_change_is_hot() {
        let old = base();
        let mut new = base();
        new.cost.rate_limit.max_requests += 1;
        new.cost.spending_limits.daily_usd = Some(baybo_model::MicroUsd::from_micros(5_000_000));
        assert!(hot_reload_diff(&old, &new).is_ok());
    }

    #[test]
    fn permission_change_is_hot() {
        let old = base();
        let mut new = base();
        new.permission = crate::PermissionPolicy::Manual;
        assert!(hot_reload_diff(&old, &new).is_ok());
    }

    #[test]
    fn model_tiers_change_is_hot() {
        let old = base();
        let mut new = base();
        new.agent
            .model_tiers
            .insert(ModelTier::Fast, LlmEntryName::from("primary"));
        assert!(hot_reload_diff(&old, &new).is_ok());
    }

    #[test]
    fn gateway_change_is_rejected() {
        let old = base();
        let mut new = base();
        new.gateway.port = old.gateway.port.wrapping_add(1);
        match hot_reload_diff(&old, &new) {
            Err(ConfigError::NotHotReloadable { section }) => assert_eq!(section, "gateway"),
            other => panic!("expected gateway rejection, got {other:?}"),
        }
    }

    #[test]
    fn agent_non_tier_change_is_rejected() {
        let old = base();
        let mut new = base();
        new.agent.max_iterations += 1;
        match hot_reload_diff(&old, &new) {
            Err(ConfigError::NotHotReloadable { section }) => {
                assert_eq!(section, "agent.max_iterations")
            }
            other => panic!("expected agent.max_iterations rejection, got {other:?}"),
        }
    }

    #[test]
    fn security_change_is_rejected() {
        let old = base();
        let mut new = base();
        new.security.leak_detection_enabled = !old.security.leak_detection_enabled;
        match hot_reload_diff(&old, &new) {
            Err(ConfigError::NotHotReloadable { section }) => assert_eq!(section, "security"),
            other => panic!("expected security rejection, got {other:?}"),
        }
    }

    #[test]
    fn handle_round_trip() {
        let handle = ConfigHandle::new(Arc::new(base()));
        assert_eq!(handle.current().default_llm.as_str(), "primary");
        let mut next = base();
        next.default_llm = LlmEntryName::from("primary2");
        handle.store(Arc::new(next));
        assert_eq!(handle.current().default_llm.as_str(), "primary2");
    }
}
