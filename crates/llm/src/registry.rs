use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::LlmClient;
use crate::providers::{
    anthropic::AnthropicProviderFactory, gemini::GeminiProviderFactory,
    minimax::MiniMaxProviderFactory, openai::OpenAIProviderFactory,
};

/// Configuration for creating an LLM provider client.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// A factory that knows how to create an `LlmClient` for a specific provider.
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
}

/// One entry in the `list_models()` aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModels {
    pub provider: String,
    pub models: Vec<String>,
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
        registry
    }

    /// Registers a provider factory. If a factory with the same name already
    /// exists, it is replaced.
    pub(crate) fn register(&mut self, factory: impl LlmProviderFactory + 'static) {
        self.factories
            .insert(factory.provider_name().to_string(), Box::new(factory));
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

    /// Creates an `LlmClient` using the factory that matches `config.provider`.
    /// Applies `config.supports_vision` as a post-factory override so each
    /// individual provider doesn't have to forward the flag.
    pub fn create_client(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
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
