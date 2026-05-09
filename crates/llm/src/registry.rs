use std::collections::HashMap;
use std::sync::Arc;

use aura_security::SecretVault;
use serde::{Deserialize, Serialize};

use crate::providers::{
    anthropic::AnthropicProviderFactory, gemini::GeminiProviderFactory,
    minimax::MiniMaxProviderFactory, openai::OpenAIProviderFactory,
    openai_subscription::OpenAiSubscriptionProviderFactory,
};
use crate::{BlobFetcher, GuardedLlm, LlmCallGuard, LlmClient, LlmCompletion};

/// Configuration for creating an LLM provider client.
#[derive(Clone, Serialize, Deserialize)]
pub struct LlmProviderConfig {
    pub provider: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: String,
    /// Operator override for the factory's default `supports_vision`.
    /// `None` keeps the factory default; `Some` forces the flag.
    /// Surfaces the corresponding field on `aura_config::LlmConfig`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    /// Reasoning effort for Codex Responses (`openai-subscription`).
    /// One of `none`/`minimal`/`low`/`medium`/`high`/`xhigh`. `None`
    /// = provider default. Other providers ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Encrypted secret store. Required by providers that hold OAuth bearers
    /// (only `openai-subscription` for now); the rest ignore it. Skipped
    /// during serialization — vault is process-local state, not config.
    #[serde(skip)]
    pub vault: Option<Arc<SecretVault>>,
}

impl std::fmt::Debug for LlmProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl: `api_key` carries the resolved bearer (raw secret) and
        // `vault` holds the master encryption key — both must stay out of
        // `tracing::debug!` and `dbg!`. Replacing the previous derive that
        // would have printed the API key verbatim.
        f.debug_struct("LlmProviderConfig")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("supports_vision", &self.supports_vision)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("vault", &self.vault.as_ref().map(|_| "<vault>"))
            .finish()
    }
}

/// A factory that knows how to create an `LlmClient` for a specific provider.
#[async_trait::async_trait]
pub trait LlmProviderFactory: Send + Sync {
    /// Returns the provider name this factory handles (e.g. `"openai"`, `"anthropic"`).
    fn provider_name(&self) -> &str;

    /// Creates an `LlmClient` from the given configuration.
    fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient>;

    /// Model ids this provider can accept as `config.model`.
    ///
    /// Advisory only — used by operator tooling (`aura llm models`) to show
    /// the choice set. Providers that cannot enumerate their catalog return
    /// an empty slice.
    fn known_models(&self) -> &'static [&'static str] {
        &[]
    }

    /// Static pricing table for this provider's known models. Used by
    /// `CostManager` to attribute spend to non-active models (e.g. a
    /// session that started under one model and got config-flipped
    /// mid-flight). Default impl pairs each `known_models()` entry
    /// with [`Self::pricing_for_model`].
    fn known_pricings(&self) -> HashMap<String, crate::ModelPricing> {
        self.known_models()
            .iter()
            .map(|m| ((*m).to_string(), self.pricing_for_model(m)))
            .collect()
    }

    /// Per-model rate this factory advertises. Default impl resolves
    /// against the bundled OpenRouter snapshot via [`Self::provider_name`]
    /// + `crate::openrouter::pricing_for`, falling back to
    /// [`Self::flat_default_pricing`] when the slug isn't in the snapshot
    /// or the provider isn't OpenRouter-routable.
    fn pricing_for_model(&self, model_id: &str) -> crate::ModelPricing {
        crate::openrouter::pricing_for(self.provider_name(), model_id)
            .unwrap_or_else(|| self.flat_default_pricing())
    }

    /// Per-provider rate kept as the unknown-id fallback in
    /// [`Self::pricing_for_model`]. Default is zero — fine for
    /// providers that bill against a flat subscription
    /// (`openai-subscription`) and for any provider whose factory
    /// doesn't (yet) ship per-token pricing.
    fn flat_default_pricing(&self) -> crate::ModelPricing {
        crate::ModelPricing::default()
    }

    /// Live discovery: ask the provider's catalog endpoint what models the
    /// caller actually has access to **right now**.
    ///
    /// Default implementation maps [`Self::known_models`] to bare
    /// [`LiveModelInfo`] entries (id only) — a no-network fallback for
    /// providers that don't expose a discovery endpoint or aren't worth
    /// implementing one for. Providers with real catalog endpoints
    /// (currently `openai-subscription` against `<base>/codex/models`)
    /// override this to return rich metadata.
    async fn live_models(&self, _config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
        Ok(self
            .known_models()
            .iter()
            .map(|id| LiveModelInfo {
                id: (*id).to_string(),
                ..Default::default()
            })
            .collect())
    }
}

/// One entry in the static `list_models()` aggregate (advertised models).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModels {
    pub provider: String,
    pub models: Vec<String>,
}

/// One entry in the live `list_live_models()` aggregate. Richer than
/// [`ProviderModels`] — providers with real catalog endpoints fill in
/// the optional fields. The default `live_models()` impl emits only
/// `id`, which gives the same data as the static `known_models()` path
/// but in the same shape so callers can render both uniformly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveModelInfo {
    /// Model slug (the value users put in `LlmConfig.model`).
    pub id: String,
    /// Human-friendly display name, when the provider supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Free-text description (often blank).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Maximum input + output tokens the model accepts. None = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Whether the model accepts image / multimodal input. None = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    /// Whether the model supports function/tool calling. None = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tools: Option<bool>,
    /// Catch-all for provider-specific metadata that doesn't fit the
    /// common shape (Codex returns ~25 fields per model — most are
    /// uninteresting to aura but kept here for operators who care).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub extras: serde_json::Value,
}

/// A registry of LLM provider factories.
///
/// Provider factories are registered by name, and clients are created
/// by looking up the factory matching the config's `provider` field.
pub struct LlmProviderRegistry {
    factories: HashMap<String, Box<dyn LlmProviderFactory>>,
}

impl LlmProviderRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Creates a registry preloaded with the built-in providers.
    pub fn with_default_providers() -> Self {
        let mut registry = Self::new();
        registry.register(OpenAIProviderFactory);
        registry.register(AnthropicProviderFactory);
        registry.register(GeminiProviderFactory);
        registry.register(MiniMaxProviderFactory);
        registry.register(OpenAiSubscriptionProviderFactory);
        registry
    }

    /// Registers a provider factory. If a factory with the same name already
    /// exists, it is replaced.
    pub(crate) fn register(&mut self, factory: impl LlmProviderFactory + 'static) {
        self.factories
            .insert(factory.provider_name().to_string(), Box::new(factory));
    }

    /// Aggregate every registered factory's `known_pricings()` into a
    /// single `model_id -> ModelPricing` map. Used by the runtime to
    /// seed `CostManager` so spans from non-active models still
    /// resolve to real USD instead of 0.0. Conflicting model-id entries
    /// across providers are last-wins; in practice the model-id space
    /// is provider-disjoint so this doesn't matter.
    ///
    /// See `LlmProviderFactory::known_pricings` for the per-model
    /// pricing accuracy caveat.
    pub fn all_known_pricings(&self) -> HashMap<String, crate::ModelPricing> {
        let mut all = HashMap::new();
        for factory in self.factories.values() {
            for (id, pricing) in factory.known_pricings() {
                all.insert(id, pricing);
            }
        }
        all
    }

    /// Return the catalog advertised by each registered factory.
    ///
    /// Output is sorted by provider name for stable display.
    pub fn list_models(&self) -> Vec<ProviderModels> {
        let mut out: Vec<ProviderModels> = self
            .factories
            .values()
            .map(|f| ProviderModels {
                provider: f.provider_name().to_string(),
                models: f.known_models().iter().map(|s| (*s).to_string()).collect(),
            })
            .collect();
        out.sort_by(|a, b| a.provider.cmp(&b.provider));
        out
    }

    /// Live-discovery counterpart to [`Self::list_models`]: ask one named
    /// provider's factory for its current catalog. Network-bound for
    /// providers that override [`LlmProviderFactory::live_models`];
    /// instant for providers that fall through to the default impl.
    ///
    /// Single-provider (not "all providers") because each one needs its
    /// own credentials/config, and aggregating across providers would
    /// require either parallel auth setups or skipping ones that lack
    /// credentials. Operators who want everything can iterate themselves.
    pub async fn list_live_models(
        &self,
        config: &LlmProviderConfig,
    ) -> crate::Result<Vec<LiveModelInfo>> {
        let factory = self.factories.get(&config.provider).ok_or_else(|| {
            crate::LlmError::ModelNotFound(format!("unknown LLM provider: {}", config.provider))
        })?;
        factory.live_models(config).await
    }

    /// **The** public production constructor. Builds the raw client
    /// internally, attaches an optional blob fetcher, and wraps the
    /// whole thing in a [`GuardedLlm`] sealed by `guard`. Returns
    /// `Arc<GuardedLlm>` so callers can fan it out to consumers
    /// without ever holding a raw `LlmClient` / `Arc<dyn
    /// LlmCompletion>` at a public boundary.
    ///
    /// `blob_fetcher` is optional; passing `None` is correct for
    /// text-only deployments and one-shot probes. `guard` is supplied
    /// by the caller — production wires `aura_agent::CostManager`'s
    /// guard, while CLI / test fixtures pass
    /// `Arc::new(|| Ok(()))` (or use [`GuardedLlm::passthrough`] /
    /// [`crate::guard::LlmCallGuard`] directly).
    pub fn create_client(
        &self,
        config: &LlmProviderConfig,
        blob_fetcher: Option<Arc<dyn BlobFetcher>>,
        guard: LlmCallGuard,
    ) -> crate::Result<Arc<GuardedLlm>> {
        let mut client = self.build_client(config)?;
        if let Some(fetcher) = blob_fetcher {
            client = client.with_blob_fetcher(fetcher);
        }
        let inner: Arc<dyn LlmCompletion> = Arc::new(client);
        Ok(GuardedLlm::new(inner, guard))
    }

    /// Internal raw-client construction. Kept `pub(crate)` because
    /// every external call should go through [`Self::create_client`]
    /// (which seals the result in a `GuardedLlm`). The internal tests
    /// in this crate still use this directly to exercise provider
    /// factories without dragging the guard layer in.
    pub(crate) fn build_client(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
        let factory = self.factories.get(&config.provider).ok_or_else(|| {
            crate::LlmError::ModelNotFound(format!("unknown LLM provider: {}", config.provider))
        })?;
        let mut client = factory.create(config)?;
        if let Some(override_) = config.supports_vision {
            client.model_info.supports_vision = override_;
        }
        Ok(client)
    }
}

impl Default for LlmProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub factory whose `known_models()` returns a fixed slice but
    /// which doesn't override `live_models` — exercises the default
    /// trait impl that maps known_models into bare LiveModelInfo entries.
    struct StubFactory;

    #[async_trait::async_trait]
    impl LlmProviderFactory for StubFactory {
        fn provider_name(&self) -> &str {
            "stub"
        }
        fn create(&self, _cfg: &LlmProviderConfig) -> crate::Result<LlmClient> {
            unreachable!("not exercised in this test")
        }
        fn known_models(&self) -> &'static [&'static str] {
            &["alpha", "beta"]
        }
        // Deliberately do NOT override live_models — that's the point of
        // the test.
    }

    #[test]
    fn all_known_pricings_aggregates_default_providers() {
        // Pin: every default provider that publishes known_pricings()
        // shows up in the aggregate. Used by `runtime::wire_router` to
        // seed `CostManager` so non-active models still attribute
        // spend instead of falling to $0.
        let registry = LlmProviderRegistry::with_default_providers();
        let pricings = registry.all_known_pricings();
        // openai, anthropic, gemini, minimax all publish known_pricings;
        // openai_subscription's known_models is empty (account-tier
        // dependent), so it contributes nothing — that's expected.
        assert!(pricings.contains_key("gpt-4o"));
        assert!(pricings.contains_key("claude-sonnet-4-6"));
        assert!(pricings.contains_key("gemini-2.5-flash"));
        assert!(pricings.contains_key("MiniMax-M2"));
        // No model id should map to (0.0, 0.0) — that would mean a
        // provider regressed its pricing publication.
        for (id, p) in &pricings {
            assert!(
                p.input_per_1m_tokens > aura_model::MicroUsd::ZERO
                    || p.output_per_1m_tokens > aura_model::MicroUsd::ZERO,
                "model {id} has zero pricing in known_pricings",
            );
        }
    }

    #[test]
    fn known_pricings_differentiates_within_provider() {
        // Pin the snapshot wiring: a regression that puts every
        // openai model back at the same flat rate would have
        // `gpt-5-mini` and `gpt-5` matching, defeating the whole
        // point of `LlmProviderFactory::openrouter_slug`.
        let registry = LlmProviderRegistry::with_default_providers();
        let pricings = registry.all_known_pricings();
        let flagship = pricings.get("gpt-5").expect("gpt-5 published");
        let mini = pricings.get("gpt-5-mini").expect("gpt-5-mini published");
        assert!(
            mini.input_per_1m_tokens < flagship.input_per_1m_tokens,
            "gpt-5-mini input must be cheaper than gpt-5: mini={:?} flagship={:?}",
            mini.input_per_1m_tokens,
            flagship.input_per_1m_tokens,
        );

        let opus = pricings
            .get("claude-opus-4-6")
            .expect("claude-opus-4-6 published");
        let haiku = pricings
            .get("claude-haiku-4-5-20251001")
            .expect("claude-haiku-4-5-20251001 published");
        assert!(
            haiku.input_per_1m_tokens < opus.input_per_1m_tokens,
            "haiku must be cheaper than opus: haiku={:?} opus={:?}",
            haiku.input_per_1m_tokens,
            opus.input_per_1m_tokens,
        );
    }

    #[tokio::test]
    async fn live_models_default_impl_maps_known_models_to_bare_entries() {
        let factory = StubFactory;
        let cfg = LlmProviderConfig {
            provider: "stub".into(),
            api_key: None,
            base_url: None,
            model: "alpha".into(),
            supports_vision: None,
            reasoning_effort: None,
            vault: None,
        };
        let entries = factory.live_models(&cfg).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "alpha");
        assert_eq!(entries[1].id, "beta");
        // All optional fields default to None — the default impl can't
        // know richer metadata, only ids.
        for e in &entries {
            assert!(e.display_name.is_none());
            assert!(e.context_window.is_none());
            assert!(e.supports_vision.is_none());
            assert!(e.supports_tools.is_none());
            assert!(e.extras.is_null());
        }
    }
}
