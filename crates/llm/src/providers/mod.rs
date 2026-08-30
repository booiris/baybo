// Internal: rig-backed providers stay crate-private — they're consumed
// only through `LlmProviderRegistry::with_default_providers()`.
pub(crate) mod anthropic;
pub(crate) mod gemini;
pub(crate) mod minimax;
pub(crate) mod openai;
pub(crate) mod rig_providers;

// Public: the subscription provider's OAuth surface (PKCE / device-code
// flows, vault token store) is consumed by `baybo-cli` for the OAuth
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

/// Which reasoning-effort dialect each provider speaks. Keyed by provider
/// name, like [`factory_defaults_for`], because both the client and the
/// dashboard's model list need the answer and only one of them has a client.
///
/// A provider absent from this table is [`EffortWire::Unwired`] — it receives
/// no effort at all. That is the safe direction (a request identical to one
/// made before effort existed), and
/// `registry::tests::every_registered_provider_declares_an_effort_dialect`
/// keeps absence from being an accident.
const EFFORT_WIRES: &[(&str, crate::effort::EffortWire)] = {
    use crate::effort::EffortWire::*;
    &[
        // OpenAI Chat Completions dialect and the providers mirroring it.
        // The inference hosts are listed by what their served models take,
        // and a level only rides when an operator asked for one.
        ("openai", OpenAiCompatible),
        ("deepseek", OpenAiCompatible),
        ("xai", OpenAiCompatible),
        ("moonshot", OpenAiCompatible),
        ("zai", OpenAiCompatible),
        ("groq", OpenAiCompatible),
        ("together", OpenAiCompatible),
        ("hyperbolic", OpenAiCompatible),
        // Anthropic Messages API — MiniMax rides its compatible surface.
        ("anthropic", Anthropic),
        ("minimax", Anthropic),
        ("gemini", Gemini),
        // Builds its own Codex Responses body; takes the level directly.
        (openai_subscription::PROVIDER_NAME, ProviderNative),
        // Declared, deliberately: no effort wiring here yet.
        ("mistral", Unwired),
        ("cohere", Unwired),
        ("perplexity", Unwired),
        ("xiaomimimo", Unwired),
        ("ollama", Unwired),
        ("llamafile", Unwired),
        ("huggingface", Unwired),
    ]
};

/// The effort dialect `provider` speaks. See [`EFFORT_WIRES`].
pub fn effort_wire_for_provider(provider: &str) -> crate::effort::EffortWire {
    EFFORT_WIRES
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, wire)| *wire)
        .unwrap_or(crate::effort::EffortWire::Unwired)
}

/// Whether `provider` has a row in [`EFFORT_WIRES`] at all — distinct from
/// resolving to `Unwired`, which is also a declaration. Exists only for the
/// coverage test that catches a new factory nobody classified.
#[cfg(test)]
fn effort_wire_is_declared(provider: &str) -> bool {
    EFFORT_WIRES.iter().any(|(name, _)| *name == provider)
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
        // Codex Responses models advertise image input in the live
        // catalog, and the subscription converter emits `input_image`.
        openai_subscription::PROVIDER_NAME => true,
        _ => false,
    };

    FactoryDefaults {
        context_window: DEFAULT_CONTEXT_WINDOW,
        supports_vision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

#[cfg(test)]
mod effort_wire_tests {
    use super::*;

    /// A provider with no row falls through to `Unwired` and silently stops
    /// honouring the operator's setting — which is exactly how effort came to
    /// be dropped everywhere in the first place. Registering a factory has to
    /// mean classifying it.
    #[test]
    fn every_registered_provider_declares_an_effort_dialect() {
        let registry = crate::LlmProviderRegistry::with_default_providers();
        let undeclared: Vec<&str> = registry
            .provider_names()
            .into_iter()
            .filter(|name| !effort_wire_is_declared(name))
            .collect();
        assert!(
            undeclared.is_empty(),
            "these providers have no EFFORT_WIRES row — add one (`Unwired` is a valid answer): {undeclared:?}"
        );
    }

    /// The reverse direction: a row for a provider that no longer exists is
    /// dead weight that reads as coverage.
    #[test]
    fn every_declared_provider_is_registered() {
        let registry = crate::LlmProviderRegistry::with_default_providers();
        let names = registry.provider_names();
        let stale: Vec<&str> = EFFORT_WIRES
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !names.iter().any(|n| n == name))
            .collect();
        assert!(
            stale.is_empty(),
            "EFFORT_WIRES rows for absent providers: {stale:?}"
        );
    }
}
