// Aura provider name -> OpenRouter catalog prefix. The single source of
// truth for this mapping, `include!`d verbatim by both `build.rs` (which
// generates the per-provider flat-default pricing from the bundled
// snapshot) and the runtime slug mapper in `crate::openrouter`. Because
// `build.rs` compiles this in isolation, the file must stay
// dependency-free: plain `&'static str` data and a lookup, nothing else.

// `(aura_provider_name, openrouter_prefix)`. Providers absent here don't
// route through OpenRouter (`openai-subscription` bills a flat OAuth
// subscription, not per-token) and so have no snapshot-derived catalog.
pub(crate) const OPENROUTER_PROVIDER_PREFIXES: &[(&str, &str)] = &[
    ("openai", "openai"),
    ("anthropic", "anthropic"),
    ("minimax", "minimax"),
    ("deepseek", "deepseek"),
    ("gemini", "google"),
    ("xai", "x-ai"),
    ("mistral", "mistralai"),
    ("cohere", "cohere"),
    ("perplexity", "perplexity"),
    ("moonshot", "moonshotai"),
    ("zai", "z-ai"),
    ("xiaomimimo", "xiaomi"),
];

// OpenRouter catalog prefix for an Aura provider, or `None` when the
// provider isn't OpenRouter-routable.
pub(crate) fn openrouter_provider_prefix(provider: &str) -> Option<&'static str> {
    OPENROUTER_PROVIDER_PREFIXES
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|&(_, prefix)| prefix)
}
