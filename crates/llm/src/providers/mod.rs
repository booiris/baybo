// Internal: rig-backed providers stay crate-private — they're consumed
// only through `LlmProviderRegistry::with_default_providers()`.
pub(crate) mod anthropic;
pub(crate) mod gemini;
pub(crate) mod minimax;
pub(crate) mod openai;
pub(crate) mod rig_providers;

// Public: the subscription provider's OAuth surface (PKCE / device-code
// flows, vault token store) is consumed by `aura-cli` for the OAuth
// branch of `aura llm add` / `aura llm edit` / `aura llm remove`, so
// the module needs to be reachable as
// `aura_llm::providers::openai_subscription`.
pub mod openai_subscription;

// Aura provider name -> OpenRouter prefix, shared with `build.rs`.
pub(crate) mod openrouter_prefix;

/// Per-provider flat-default pricing, generated at build time from the
/// bundled OpenRouter snapshot (see `build.rs`): the priciest snapshot
/// model under the provider's prefix by `input + output`.
pub(crate) mod catalog {
    use crate::ModelPricing;
    use aura_model::MicroUsd;

    include!(concat!(env!("OUT_DIR"), "/provider_catalog.rs"));
}

/// Per-provider built-in fallbacks for `(context_window, supports_vision)`.
/// The single source of truth: each factory layers `OpenRouter snapshot
/// → operator override → these defaults`, and the gateway dashboard
/// uses the same table to render "effective" values when nothing else
/// is known. Without this consolidation the dashboard had to mirror
/// five `unwrap_or(...)` literals by hand and silently flipped
/// `supports_vision` `true` for minimax / openai-subscription where
/// the runtime defaults to `false`.
#[derive(Debug, Clone, Copy)]
pub struct FactoryDefaults {
    pub context_window: usize,
    pub supports_vision: bool,
}

pub fn factory_defaults_for(provider: &str) -> FactoryDefaults {
    match provider {
        "openai" => FactoryDefaults {
            context_window: 128_000,
            supports_vision: true,
        },
        "anthropic" => FactoryDefaults {
            context_window: 200_000,
            supports_vision: true,
        },
        "gemini" => FactoryDefaults {
            context_window: 1_000_000,
            supports_vision: true,
        },
        "minimax" => FactoryDefaults {
            context_window: 200_000,
            supports_vision: false,
        },
        // DeepSeek V3/R1 family: 128k context, text-only.
        "deepseek" => FactoryDefaults {
            context_window: 128_000,
            supports_vision: false,
        },
        // Subscription billing is account-level, vision-off is a
        // hard product fact for the Codex catalog.
        openai_subscription::PROVIDER_NAME => FactoryDefaults {
            context_window: 272_000,
            supports_vision: false,
        },
        _ => FactoryDefaults {
            context_window: 128_000,
            supports_vision: true,
        },
    }
}
