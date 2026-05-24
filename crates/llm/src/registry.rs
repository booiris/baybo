use std::collections::HashMap;
use std::sync::Arc;

use aura_security::SecretVault;
use serde::{Deserialize, Serialize};

use crate::providers::{
    anthropic::AnthropicProviderFactory, deepseek::DeepSeekProviderFactory,
    gemini::GeminiProviderFactory, minimax::MiniMaxProviderFactory, openai::OpenAIProviderFactory,
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
    /// Operator override for `ModelInfo.context_window`. `None` keeps
    /// the factory default (OpenRouter snapshot, then per-provider
    /// constant); `Some` clamps the active context budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Operator override for per-token pricing fields. Each field is
    /// independently optional — unset fields keep the factory default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<LlmPricingOverride>,
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

pub use aura_model::LlmPricingOverride;

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
            .field("context_window", &self.context_window)
            .field("pricing", &self.pricing)
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
    /// Default implementation maps the bundled OpenRouter snapshot's
    /// catalog for this provider ([`crate::openrouter::snapshot_model_ids_for`])
    /// to bare [`LiveModelInfo`] entries (id only) — a no-network fallback
    /// for providers that don't expose a discovery endpoint or aren't
    /// worth implementing one for. Returns empty for providers that don't
    /// route through OpenRouter. Providers with real catalog endpoints
    /// (currently `openai-subscription` against `<base>/codex/models`)
    /// override this to return rich metadata.
    async fn live_models(&self, _config: &LlmProviderConfig) -> crate::Result<Vec<LiveModelInfo>> {
        Ok(
            crate::openrouter::snapshot_model_ids_for(self.provider_name())
                .into_iter()
                .map(|id| LiveModelInfo {
                    id,
                    ..Default::default()
                })
                .collect(),
        )
    }
}

/// One entry in the live `list_live_models()` aggregate. Providers with
/// real catalog endpoints fill in the optional fields; the default
/// `live_models()` impl (the OpenRouter snapshot) emits only `id`.
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
        registry.register(DeepSeekProviderFactory);
        registry.register(OpenAiSubscriptionProviderFactory);
        registry
    }

    /// Registers a provider factory. If a factory with the same name already
    /// exists, it is replaced.
    pub(crate) fn register(&mut self, factory: impl LlmProviderFactory + 'static) {
        self.factories
            .insert(factory.provider_name().to_string(), Box::new(factory));
    }

    /// Ask one named provider's factory for its current catalog.
    /// Network-bound for providers that override
    /// [`LlmProviderFactory::live_models`];
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
        let live = factory.live_models(config).await?;
        // Vendor-side `/v1/models` endpoints aren't always populated
        // (e.g. MiniMax's China cluster returns `{"data":null}`). When
        // that happens the user would otherwise be forced into manual
        // entry — fall back to the bundled OpenRouter catalog so every
        // provider gets a pickable list. Errors propagate; only a clean
        // empty Ok triggers the fallback.
        if live.is_empty() {
            let mut snapshot = crate::openrouter::snapshot_model_ids_for(&config.provider)
                .into_iter()
                .map(|id| LiveModelInfo {
                    id,
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            snapshot.sort_by(|a, b| a.id.cmp(&b.id));
            return Ok(snapshot);
        }
        Ok(live)
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
        if let Some(ctx) = config.context_window {
            client.model_info.context_window = ctx;
        }
        if let Some(p) = config.pricing {
            if let Some(v) = p.input_per_1m_tokens {
                client.model_info.pricing.input_per_1m_tokens = v;
            }
            if let Some(v) = p.output_per_1m_tokens {
                client.model_info.pricing.output_per_1m_tokens = v;
            }
            if p.cached_input_per_1m_tokens.is_some() {
                client.model_info.pricing.cached_input_per_1m_tokens = p.cached_input_per_1m_tokens;
            }
            if p.cache_write_per_1m_tokens.is_some() {
                client.model_info.pricing.cache_write_per_1m_tokens = p.cache_write_per_1m_tokens;
            }
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

    /// Stub factory that doesn't override `live_models` — exercises the
    /// default trait impl that maps the OpenRouter snapshot catalog into
    /// bare LiveModelInfo entries. `provider` selects which catalog.
    struct StubFactory {
        provider: &'static str,
    }

    #[async_trait::async_trait]
    impl LlmProviderFactory for StubFactory {
        fn provider_name(&self) -> &str {
            self.provider
        }
        fn create(&self, _cfg: &LlmProviderConfig) -> crate::Result<LlmClient> {
            unreachable!("not exercised in this test")
        }
        // Deliberately do NOT override live_models — that's the point of
        // the test.
    }

    fn bare_config(provider: &str) -> LlmProviderConfig {
        LlmProviderConfig {
            provider: provider.into(),
            api_key: None,
            base_url: None,
            model: "unused".into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
        }
    }

    #[tokio::test]
    async fn live_models_default_impl_maps_snapshot_to_bare_entries() {
        // Snapshot-routable provider: the default impl surfaces its
        // OpenRouter catalog as id-only entries.
        let factory = StubFactory {
            provider: "deepseek",
        };
        let entries = factory.live_models(&bare_config("deepseek")).await.unwrap();
        assert!(
            entries.iter().any(|e| e.id == "deepseek-chat"),
            "expected snapshot-derived deepseek ids, got {:?}",
            entries.iter().map(|e| &e.id).collect::<Vec<_>>(),
        );
        // id-only: the default impl can't know richer metadata.
        for e in &entries {
            assert!(e.display_name.is_none());
            assert!(e.context_window.is_none());
            assert!(e.supports_vision.is_none());
            assert!(e.supports_tools.is_none());
            assert!(e.extras.is_null());
        }
        // A provider that doesn't route through OpenRouter yields nothing.
        let empty = StubFactory { provider: "stub" }
            .live_models(&bare_config("stub"))
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    /// Empty-live factory registered under a snapshot-routable provider
    /// name — exercises the registry-level fallback that swaps an empty
    /// `live_models` response for the OpenRouter snapshot's catalog.
    struct EmptyLiveFactory {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl LlmProviderFactory for EmptyLiveFactory {
        fn provider_name(&self) -> &str {
            self.name
        }
        fn create(&self, _cfg: &LlmProviderConfig) -> crate::Result<LlmClient> {
            unreachable!("not exercised in this test")
        }
        async fn live_models(
            &self,
            _config: &LlmProviderConfig,
        ) -> crate::Result<Vec<LiveModelInfo>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn list_live_models_falls_back_to_openrouter_snapshot_when_empty() {
        // Provider whose `/v1/models` returns nothing (the MiniMax China
        // cluster's actual behavior) — registry must surface the bundled
        // OpenRouter slugs so setup still has something to show.
        let mut registry = LlmProviderRegistry::new();
        registry.register(EmptyLiveFactory { name: "minimax" });
        let cfg = LlmProviderConfig {
            provider: "minimax".into(),
            api_key: None,
            base_url: None,
            model: "unused".into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
        };
        let entries = registry.list_live_models(&cfg).await.unwrap();
        assert!(
            !entries.is_empty(),
            "expected snapshot fallback to populate minimax catalog",
        );
        assert!(
            entries.iter().any(|e| e.id == "minimax-m2"),
            "expected canonical minimax-m2 slug in fallback list, got {:?}",
            entries.iter().map(|e| &e.id).collect::<Vec<_>>(),
        );
        // Sorted by id so the UI render is stable.
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "fallback entries must come out sorted");
    }

    #[tokio::test]
    async fn list_live_models_does_not_clobber_non_empty_response() {
        // A provider that actually returns live entries must not be
        // overridden by the snapshot fallback — fallback only triggers on
        // an empty Ok.
        struct OneLiveFactory;
        #[async_trait::async_trait]
        impl LlmProviderFactory for OneLiveFactory {
            fn provider_name(&self) -> &str {
                "anthropic"
            }
            fn create(&self, _cfg: &LlmProviderConfig) -> crate::Result<LlmClient> {
                unreachable!()
            }
            async fn live_models(
                &self,
                _config: &LlmProviderConfig,
            ) -> crate::Result<Vec<LiveModelInfo>> {
                Ok(vec![LiveModelInfo {
                    id: "claude-from-live-api".into(),
                    ..Default::default()
                }])
            }
        }

        let mut registry = LlmProviderRegistry::new();
        registry.register(OneLiveFactory);
        let cfg = LlmProviderConfig {
            provider: "anthropic".into(),
            api_key: None,
            base_url: None,
            model: "unused".into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
        };
        let entries = registry.list_live_models(&cfg).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "claude-from-live-api");
    }
}
