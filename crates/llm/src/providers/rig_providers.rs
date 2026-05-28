//! Additional rig-backed provider factories wired through one uniform
//! builder pattern. Every rig provider exposes the same
//! `Client::builder().base_url(..).http_client(..).build()` surface and a
//! `completion_model(..)`, so a macro generates the `LlmProviderFactory`
//! impl for each — they differ only in the rig module, the
//! `AnyCompletionModel` variant, how the API key is supplied, and (for
//! model-author providers) the snapshot-backed flat-default pricing.
//!
//! `base_url` is only forwarded when the operator overrides it; otherwise
//! each provider's own default endpoint (baked into its rig `Client`) is
//! used. The proxied HTTP client is always installed so egress-proxy config
//! is honored uniformly.

use rig::client::{CompletionClient, Nothing};

use crate::providers::catalog;
use crate::registry::{LlmProviderConfig, LlmProviderFactory};
use crate::{AnyCompletionModel, LlmClient, ModelInfo, ModelPricing};

/// Build the `ModelInfo` for a rig-backed model: OpenRouter snapshot
/// capabilities → per-provider defaults, with the factory's resolved
/// pricing. Shared by every arm of [`rig_provider_factory`].
fn rig_model_info(name: &str, model_id: &str, pricing: ModelPricing) -> ModelInfo {
    let caps = crate::openrouter::capabilities_for(name, model_id);
    let defaults = crate::providers::factory_defaults_for(name);
    ModelInfo {
        id: model_id.to_string(),
        provider: name.to_string(),
        context_window: caps
            .and_then(|c| c.context_window)
            .unwrap_or(defaults.context_window),
        supports_tools: true,
        supports_vision: caps
            .and_then(|c| c.supports_vision)
            .unwrap_or(defaults.supports_vision),
        pricing,
    }
}

/// Generates an `LlmProviderFactory`. Auth modes:
/// * `priced = EXPR` — requires an API key; pins a snapshot-backed
///   flat-default pricing (model-author providers routable via OpenRouter).
/// * (no extra) — requires an API key; trait-default (zero) pricing
///   (inference hosts; operators set rates via `LlmConfig.pricing`).
/// * `optional_key` — API key optional (e.g. a local Ollama that may or
///   may not sit behind auth).
/// * `keyless` — provider takes no API key at all (e.g. llamafile).
macro_rules! rig_provider_factory {
    // --- shared skeleton: $build yields the configured rig client ---
    (@factory $factory:ident, $name:literal, $variant:ident, $build:expr $(, pricing = $pricing:expr)?) => {
        pub struct $factory;

        #[async_trait::async_trait]
        impl LlmProviderFactory for $factory {
            fn provider_name(&self) -> &str {
                $name
            }

            $(fn flat_default_pricing(&self) -> ModelPricing { $pricing })?

            fn create(&self, config: &LlmProviderConfig) -> crate::Result<LlmClient> {
                let build = $build;
                let client = build(config)?;
                let model = client.completion_model(&config.model);
                let model_info =
                    rig_model_info($name, &config.model, self.pricing_for_model(&config.model));
                Ok(LlmClient::new(model_info, AnyCompletionModel::$variant(model)))
            }
        }
    };

    // require an API key, snapshot-backed pricing
    ($factory:ident, $name:literal, $module:ident, $variant:ident, priced = $pricing:expr) => {
        rig_provider_factory!(@factory $factory, $name, $variant,
            |config: &LlmProviderConfig| -> crate::Result<rig::providers::$module::Client> {
                let api_key = config.api_key.as_deref().ok_or_else(|| {
                    crate::LlmError::Config(format!("{} requires an API key", $name))
                })?;
                let b = rig::providers::$module::Client::builder().api_key(api_key);
                let b = match config.base_url.as_deref() { Some(u) => b.base_url(u), None => b };
                b.http_client(crate::proxied_client(config.proxy.as_ref())?)
                    .build()
                    .map_err(|e| crate::LlmError::Config(format!("failed to create {} client: {e}", $name)))
            },
            pricing = $pricing);
    };

    // require an API key, zero default pricing
    ($factory:ident, $name:literal, $module:ident, $variant:ident) => {
        rig_provider_factory!(@factory $factory, $name, $variant,
            |config: &LlmProviderConfig| -> crate::Result<rig::providers::$module::Client> {
                let api_key = config.api_key.as_deref().ok_or_else(|| {
                    crate::LlmError::Config(format!("{} requires an API key", $name))
                })?;
                let b = rig::providers::$module::Client::builder().api_key(api_key);
                let b = match config.base_url.as_deref() { Some(u) => b.base_url(u), None => b };
                b.http_client(crate::proxied_client(config.proxy.as_ref())?)
                    .build()
                    .map_err(|e| crate::LlmError::Config(format!("failed to create {} client: {e}", $name)))
            });
    };

    // API key optional (local providers that may sit behind auth)
    ($factory:ident, $name:literal, $module:ident, $variant:ident, optional_key) => {
        rig_provider_factory!(@factory $factory, $name, $variant,
            |config: &LlmProviderConfig| -> crate::Result<rig::providers::$module::Client> {
                let b = rig::providers::$module::Client::builder();
                let b = match config.api_key.as_deref() {
                    Some(k) => b.api_key(k),
                    None => b.api_key(Nothing),
                };
                let b = match config.base_url.as_deref() { Some(u) => b.base_url(u), None => b };
                b.http_client(crate::proxied_client(config.proxy.as_ref())?)
                    .build()
                    .map_err(|e| crate::LlmError::Config(format!("failed to create {} client: {e}", $name)))
            });
    };

    // provider takes no API key
    ($factory:ident, $name:literal, $module:ident, $variant:ident, keyless) => {
        rig_provider_factory!(@factory $factory, $name, $variant,
            |config: &LlmProviderConfig| -> crate::Result<rig::providers::$module::Client> {
                let b = rig::providers::$module::Client::builder().api_key(Nothing);
                let b = match config.base_url.as_deref() { Some(u) => b.base_url(u), None => b };
                b.http_client(crate::proxied_client(config.proxy.as_ref())?)
                    .build()
                    .map_err(|e| crate::LlmError::Config(format!("failed to create {} client: {e}", $name)))
            });
    };
}

// Model-author providers: routable through OpenRouter, so they carry a
// snapshot-backed flat-default pricing (see `openrouter_prefix.rs` + build.rs).
rig_provider_factory!(
    XaiProviderFactory,
    "xai",
    xai,
    Xai,
    priced = catalog::XAI_FLAT_DEFAULT_PRICING
);
rig_provider_factory!(
    MistralProviderFactory,
    "mistral",
    mistral,
    Mistral,
    priced = catalog::MISTRAL_FLAT_DEFAULT_PRICING
);
rig_provider_factory!(
    CohereProviderFactory,
    "cohere",
    cohere,
    Cohere,
    priced = catalog::COHERE_FLAT_DEFAULT_PRICING
);
rig_provider_factory!(
    PerplexityProviderFactory,
    "perplexity",
    perplexity,
    Perplexity,
    priced = catalog::PERPLEXITY_FLAT_DEFAULT_PRICING
);
rig_provider_factory!(
    MoonshotProviderFactory,
    "moonshot",
    moonshot,
    Moonshot,
    priced = catalog::MOONSHOT_FLAT_DEFAULT_PRICING
);
rig_provider_factory!(
    ZaiProviderFactory,
    "zai",
    zai,
    Zai,
    priced = catalog::ZAI_FLAT_DEFAULT_PRICING
);
rig_provider_factory!(
    XiaomiMimoProviderFactory,
    "xiaomimimo",
    xiaomimimo,
    XiaomiMimo,
    priced = catalog::XIAOMIMIMO_FLAT_DEFAULT_PRICING
);

// Inference hosts: they serve many models at varying rates and aren't a
// single OpenRouter author prefix, so they ship no flat-default pricing (the
// trait default of zero). Operators set per-token rates via `LlmConfig.pricing`
// for budget accounting; without it these providers' usage records at zero cost.
rig_provider_factory!(GroqProviderFactory, "groq", groq, Groq);
rig_provider_factory!(TogetherProviderFactory, "together", together, Together);
rig_provider_factory!(
    HyperbolicProviderFactory,
    "hyperbolic",
    hyperbolic,
    Hyperbolic
);
rig_provider_factory!(
    HuggingFaceProviderFactory,
    "huggingface",
    huggingface,
    HuggingFace
);
rig_provider_factory!(
    OllamaProviderFactory,
    "ollama",
    ollama,
    Ollama,
    optional_key
);
rig_provider_factory!(
    LlamafileProviderFactory,
    "llamafile",
    llamafile,
    Llamafile,
    keyless
);

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::MicroUsd;

    fn config(provider: &str) -> LlmProviderConfig {
        LlmProviderConfig {
            provider: provider.into(),
            api_key: Some("test-key".into()),
            base_url: None,
            model: "test-model".into(),
            supports_vision: None,
            context_window: None,
            pricing: None,
            reasoning_effort: None,
            vault: None,
            proxy: None,
        }
    }

    /// Model-author factories: pin their provider name, require an API key,
    /// build a client offline, and expose a non-zero snapshot-backed
    /// flat-default pricing (a zero default would silently disable the
    /// budget gate for unknown model ids).
    macro_rules! assert_priced_factory {
        ($factory:expr, $name:literal) => {{
            let f = $factory;
            assert_eq!(f.provider_name(), $name);

            let mut no_key = config($name);
            no_key.api_key = None;
            assert!(
                f.create(&no_key).is_err(),
                "{} must require an api key",
                $name
            );

            let client = f.create(&config($name)).expect("builds with api key");
            assert_eq!(client.model_info().provider, $name);

            let p = f.pricing_for_model("definitely-not-in-the-snapshot");
            assert!(
                p.input_per_1m_tokens > MicroUsd::ZERO,
                "{} flat input pricing must be non-zero",
                $name
            );
            assert!(
                p.output_per_1m_tokens > MicroUsd::ZERO,
                "{} flat output pricing must be non-zero",
                $name
            );
        }};
    }

    #[test]
    fn model_author_factories_pin_provider_and_have_nonzero_flat_pricing() {
        assert_priced_factory!(XaiProviderFactory, "xai");
        assert_priced_factory!(MistralProviderFactory, "mistral");
        assert_priced_factory!(CohereProviderFactory, "cohere");
        assert_priced_factory!(PerplexityProviderFactory, "perplexity");
        assert_priced_factory!(MoonshotProviderFactory, "moonshot");
        assert_priced_factory!(ZaiProviderFactory, "zai");
        assert_priced_factory!(XiaomiMimoProviderFactory, "xiaomimimo");
    }

    #[test]
    fn host_factories_build_and_pin_provider() {
        // Keyed hosts build with a key.
        for (name, built) in [
            ("groq", GroqProviderFactory.create(&config("groq"))),
            (
                "together",
                TogetherProviderFactory.create(&config("together")),
            ),
            (
                "hyperbolic",
                HyperbolicProviderFactory.create(&config("hyperbolic")),
            ),
            (
                "huggingface",
                HuggingFaceProviderFactory.create(&config("huggingface")),
            ),
        ] {
            let client = built.expect("keyed host builds with api key");
            assert_eq!(client.model_info().provider, name);
        }

        // Local providers build without an API key.
        let mut keyless = config("ollama");
        keyless.api_key = None;
        assert_eq!(
            OllamaProviderFactory
                .create(&keyless)
                .expect("ollama builds keyless")
                .model_info()
                .provider,
            "ollama"
        );
        let mut llama = config("llamafile");
        llama.api_key = None;
        assert_eq!(
            LlamafileProviderFactory
                .create(&llama)
                .expect("llamafile builds keyless")
                .model_info()
                .provider,
            "llamafile"
        );
    }
}
