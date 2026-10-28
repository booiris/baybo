// Internal: rig-backed providers stay crate-private — they're consumed
// only through `LlmProviderRegistry::with_default_providers()`.
pub(crate) mod anthropic;
pub(crate) mod gemini;
pub(crate) mod minimax;
pub(crate) mod openai;
pub(crate) mod openai_responses;
pub(crate) mod opencode;
pub(crate) mod rig_providers;

// Public: the subscription provider's OAuth surface (browser PKCE login,
// vault token store) is consumed by `baybo-cli` for the OAuth
// branch of `baybo llm add` / `baybo llm edit` / `baybo llm remove`, so
// the module needs to be reachable as
// `baybo_llm::providers::openai_subscription`.
pub mod openai_subscription;

// Baybo provider name -> OpenRouter prefix, shared with `build.rs`.
pub(crate) mod openrouter_prefix;

/// Per-provider flat-default pricing, generated at build time from the
/// bundled OpenRouter snapshot (see `build.rs`): the priciest snapshot
/// model under the provider's prefix by `input + output`.
pub(crate) mod catalog {
    use crate::ModelPricing;
    use baybo_model::MicroUsd;

    include!(concat!(env!("OUT_DIR"), "/provider_catalog.rs"));
}

/// Resolve the model's effort dialect through its registered factory.
pub fn effort_wire_for_model(provider: &str, model: &str) -> crate::effort::EffortWire {
    crate::default_registry()
        .factory_for(provider)
        .map(|factory| factory.effort_wire(model))
        .unwrap_or(crate::effort::EffortWire::Unwired)
}

/// Per-provider built-in fallbacks for `(context_window, supports_vision)`.
/// The single source of truth: each factory layers `OpenRouter snapshot
/// → operator override → these defaults`, and the gateway dashboard
/// uses the same table to render "effective" values when nothing else
/// is known. Without this consolidation the dashboard had to mirror
/// five `unwrap_or(...)` literals by hand.
#[derive(Debug, Clone, Copy)]
pub struct FactoryDefaults {
    pub context_window: usize,
    pub supports_vision: bool,
}

const DEFAULT_CONTEXT_WINDOW: usize = 256_000;

pub fn factory_defaults_for(provider: &str) -> FactoryDefaults {
    let supports_vision = match provider {
        "openai" | "anthropic" | "gemini" => true,
        "minimax" | "deepseek" => false,
        // Subscription Responses models advertise image input in the live
        // catalog, and the subscription converter emits `input_image`.
        openai_subscription::PROVIDER_NAME => true,
        _ => false,
    };

    FactoryDefaults {
        context_window: DEFAULT_CONTEXT_WINDOW,
        supports_vision,
    }
}

/// Model-specific defaults, shared by client construction and dashboard metadata.
pub fn factory_defaults_for_model(provider: &str, model: &str) -> FactoryDefaults {
    crate::default_registry()
        .factory_for(provider)
        .map(|factory| factory.model_defaults(model))
        .unwrap_or_else(|| factory_defaults_for(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_provider_metadata_stays_conservative() {
        assert_eq!(
            effort_wire_for_model("unknown-provider", "gpt-example"),
            crate::effort::EffortWire::Unwired
        );
        let defaults = factory_defaults_for_model("unknown-provider", "gpt-example");
        assert!(!defaults.supports_vision);
        assert_eq!(defaults.context_window, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn vision_defaults_are_conservative_and_subscription_is_enabled() {
        assert!(factory_defaults_for("openai").supports_vision);
        assert!(factory_defaults_for(openai_subscription::PROVIDER_NAME).supports_vision);
        assert!(!factory_defaults_for("unknown-provider").supports_vision);
    }

    #[test]
    fn every_provider_uses_the_shared_context_window_default() {
        for provider in [
            "openai",
            "anthropic",
            "gemini",
            "minimax",
            "deepseek",
            openai_subscription::PROVIDER_NAME,
            "unknown-provider",
        ] {
            assert_eq!(
                factory_defaults_for(provider).context_window,
                DEFAULT_CONTEXT_WINDOW,
                "provider {provider}"
            );
        }
    }
}
