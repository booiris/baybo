//! Construction from operator config, mirroring `baybo_memory::boot`.
//!
//! Every failure path returns an empty tool list and logs — never an error.
//! The rest of baybo must still come up so the operator can fix the
//! credential and restart.

use std::sync::Arc;

use baybo_config::web_search::{WebSearchConfig, WebSearchProvider};
use baybo_security::SecretVault;
use baybo_security::http::ProxySettings;
use baybo_security::user_secret::USER_SECRET_PREFIX;
use baybo_tools::{Tool, ToolManifest, ToolRegistry};

use crate::SearchProvider;
use crate::error::SearchError;
use crate::providers::{brave::BraveProvider, searxng::SearxngProvider, tavily::TavilyProvider};
use crate::tool::{WebSearchTool, WebSearchToolConfig};

/// Dynamic-registry source that owns the `WebSearch` tool.
///
/// It is a *dynamic* registration rather than a builtin one so a config
/// reload can add, replace or remove the tool on a running process, the same
/// way the MCP reconciler does. Builtin registration is single-writer at
/// startup and takes `&mut ToolRegistry`, which no longer exists once the
/// registry is frozen into an `Arc`.
pub const WEB_SEARCH_SOURCE: &str = "web_search";

/// Build the tool from `config` and make it the whole of
/// [`WEB_SEARCH_SOURCE`] — installing it, replacing it, or removing it,
/// whichever the config now calls for.
///
/// Boot and reload both go through here so there is one answer to "what does
/// this config produce", and the swap is atomic: `replace_source` takes the
/// registry's write lock once, so no turn can observe a moment where the tool
/// has been removed and not yet put back.
pub async fn install(
    registry: &ToolRegistry,
    config: &WebSearchConfig,
    vault: Option<&SecretVault>,
    proxy: Option<&ProxySettings>,
) -> bool {
    let tools = build_search_tools(config, vault, proxy).await;
    !registry.replace_source(WEB_SEARCH_SOURCE, tools).is_empty()
}

/// Resolve the configured provider's API key. Order:
///   1. User secret vault entry `user_env.<name>`
///      (managed via `baybo secret add <name>`).
///   2. Process env var of the same name.
///
/// Same mechanism as the memory backends, and for the same reason: it reuses
/// `baybo secret add/list/delete` and `baybo vault rotate` with no new CLI,
/// while a containerised deploy can still pass a plain env var.
pub async fn resolve_api_key(cfg: &WebSearchConfig, vault: Option<&SecretVault>) -> Option<String> {
    let name = cfg.resolved_api_key_name()?;
    if let Some(vault) = vault {
        let key = format!("{USER_SECRET_PREFIX}{name}");
        if let Ok(Some(secret)) = vault.get_secret(&key).await
            && let Ok(s) = secret.as_str()
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// Build the `WebSearch` tool for registration into the builtin registry.
///
/// Returns an empty vec when web search is off, has no provider, or is
/// misconfigured. Registering a verb the model can see but never execute
/// would waste prompt tokens and invite retry loops, so nothing is offered
/// unless it can actually run.
pub async fn build_search_tools(
    config: &WebSearchConfig,
    vault: Option<&SecretVault>,
    proxy: Option<&ProxySettings>,
) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    if !config.enabled {
        return Vec::new();
    }
    if config.provider == WebSearchProvider::Noop {
        tracing::warn!(
            "web_search.enabled is true but no provider is selected; the WebSearch tool is \
             not registered. Set web_search.provider to tavily, brave or searxng."
        );
        return Vec::new();
    }

    let api_key = resolve_api_key(config, vault).await;
    if config.provider.requires_api_key() && api_key.is_none() {
        // `error!`, not `warn!`: the operator asked for this feature and it
        // did not come up. A warning is for a choice; this is a defect in the
        // deployment.
        let name = config.resolved_api_key_name().unwrap_or_default();
        tracing::error!(
            provider = ?config.provider,
            "web search is enabled but no API key resolved; the WebSearch tool is not \
             registered. Run `baybo secret add {name}` or set the {name} environment variable."
        );
        return Vec::new();
    }
    // Only reachable for a provider that requires one, so the unwrap-free
    // default is the keyless case.
    let api_key = api_key.unwrap_or_default();

    let provider: Arc<dyn SearchProvider> = match build_provider(config, api_key, proxy) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                provider = ?config.provider,
                error = %e,
                "web search provider construction failed; the WebSearch tool is not registered"
            );
            return Vec::new();
        }
    };

    let tool = WebSearchTool::from_config(WebSearchToolConfig {
        provider,
        max_results: config.max_results,
        blocked_domains: config.blocked_domains.clone(),
        country: config.country.clone(),
        language: config.language.clone(),
        api_key_name: config.resolved_api_key_name().map(str::to_string),
    });
    let manifest = tool.manifest();
    tracing::info!(
        provider = ?config.provider,
        max_results = config.max_results,
        "web search enabled"
    );
    vec![(Arc::new(tool) as Arc<dyn Tool>, manifest)]
}

fn build_provider(
    config: &WebSearchConfig,
    api_key: String,
    proxy: Option<&ProxySettings>,
) -> Result<Arc<dyn SearchProvider>, SearchError> {
    let base_url = config.base_url.as_deref();
    if config.provider.requires_api_key() && base_url.is_some_and(|u| u.starts_with("http://")) {
        tracing::warn!(
            provider = ?config.provider,
            "web_search.base_url is plain http, so this provider's API key travels in \
             cleartext on every search. Use https unless the endpoint is on loopback."
        );
    }
    Ok(match config.provider {
        WebSearchProvider::Noop => {
            return Err(SearchError::Config {
                reason: "no provider selected".to_string(),
            });
        }
        WebSearchProvider::Tavily => Arc::new(TavilyProvider::new(api_key, base_url, proxy)?),
        WebSearchProvider::Brave => Arc::new(BraveProvider::new(api_key, base_url, proxy)?),
        WebSearchProvider::Searxng => {
            let base = base_url.ok_or_else(|| SearchError::Config {
                reason: "web_search.base_url is required for searxng — it has no hosted endpoint"
                    .to_string(),
            })?;
            Arc::new(SearxngProvider::new(base, proxy)?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WEB_SEARCH_TOOL_NAME;
    use baybo_tools::ToolRegistry;

    fn enabled(provider: WebSearchProvider) -> WebSearchConfig {
        WebSearchConfig {
            enabled: true,
            provider,
            ..Default::default()
        }
    }

    /// Env mutation is process-global; each name is unique to its test.
    struct EnvGuard(&'static str);

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            // SAFETY: see the Drop impl — the name is unique to this test.
            unsafe { std::env::set_var(name, value) };
            Self(name)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation is unsynchronised; this name is touched by
            // no other test in the process.
            unsafe { std::env::remove_var(self.0) };
        }
    }

    #[tokio::test]
    async fn a_disabled_section_registers_nothing() {
        let tools = build_search_tools(&WebSearchConfig::default(), None, None).await;
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn enabled_with_no_provider_registers_nothing() {
        let tools = build_search_tools(&enabled(WebSearchProvider::Noop), None, None).await;
        assert!(tools.is_empty());
    }

    /// Boot must survive a missing credential — the rest of baybo still runs.
    #[tokio::test]
    async fn a_missing_key_registers_nothing_instead_of_failing_boot() {
        let mut cfg = enabled(WebSearchProvider::Tavily);
        cfg.api_key_name = Some("BAYBO_TEST_ABSENT_SEARCH_KEY".into());
        let tools = build_search_tools(&cfg, None, None).await;
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn an_env_var_key_is_enough_to_register() {
        let _guard = EnvGuard::set("BAYBO_TEST_ENV_SEARCH_KEY", "k");
        let mut cfg = enabled(WebSearchProvider::Tavily);
        cfg.api_key_name = Some("BAYBO_TEST_ENV_SEARCH_KEY".into());

        let tools = build_search_tools(&cfg, None, None).await;
        assert_eq!(tools.len(), 1);
        let (tool, manifest) = &tools[0];
        assert_eq!(tool.name(), WEB_SEARCH_TOOL_NAME);
        assert_eq!(manifest.name, WEB_SEARCH_TOOL_NAME);
        assert!(manifest.channels.is_empty());
    }

    /// The keyless provider: no credential, but the address is mandatory.
    #[tokio::test]
    async fn searxng_needs_no_key_but_does_need_a_base_url() {
        let cfg = enabled(WebSearchProvider::Searxng);
        assert!(
            build_search_tools(&cfg, None, None).await.is_empty(),
            "no base_url means no tool"
        );

        let cfg = WebSearchConfig {
            base_url: Some("http://searxng.internal:8080".into()),
            ..enabled(WebSearchProvider::Searxng)
        };
        assert_eq!(build_search_tools(&cfg, None, None).await.len(), 1);
    }

    #[tokio::test]
    async fn a_base_url_carrying_credentials_registers_nothing() {
        let _guard = EnvGuard::set("BAYBO_TEST_BADURL_SEARCH_KEY", "k");
        let cfg = WebSearchConfig {
            api_key_name: Some("BAYBO_TEST_BADURL_SEARCH_KEY".into()),
            base_url: Some("https://user:pass@evil.tld".into()),
            ..enabled(WebSearchProvider::Tavily)
        };
        assert!(build_search_tools(&cfg, None, None).await.is_empty());
    }

    /// Boot and reload share this seam, so the register → replace → remove
    /// cycle has to work from a plain registry.
    #[tokio::test]
    async fn install_registers_replaces_and_removes_through_one_source() {
        let _guard = EnvGuard::set("BAYBO_TEST_INSTALL_SEARCH_KEY", "k");
        let registry = ToolRegistry::new();
        let mut cfg = enabled(WebSearchProvider::Tavily);
        cfg.api_key_name = Some("BAYBO_TEST_INSTALL_SEARCH_KEY".into());

        assert!(install(&registry, &cfg, None, None).await);
        assert!(registry.get(WEB_SEARCH_TOOL_NAME).is_some());
        assert_eq!(
            registry.dynamic_names_for_source(WEB_SEARCH_SOURCE),
            vec![WEB_SEARCH_TOOL_NAME.to_string()]
        );

        // Re-installing the same config must not accumulate registrations.
        assert!(install(&registry, &cfg, None, None).await);
        assert_eq!(
            registry.dynamic_names_for_source(WEB_SEARCH_SOURCE).len(),
            1
        );

        // Switching to a provider whose key is absent takes the tool away
        // rather than leaving a stale one that authenticates with the old key.
        let mut broken = enabled(WebSearchProvider::Brave);
        broken.api_key_name = Some("BAYBO_TEST_ABSENT_INSTALL_KEY".into());
        assert!(!install(&registry, &broken, None, None).await);
        assert!(registry.get(WEB_SEARCH_TOOL_NAME).is_none());

        // …and disabling does the same.
        assert!(install(&registry, &cfg, None, None).await);
        assert!(
            !install(&registry, &WebSearchConfig::default(), None, None).await,
            "a disabled section must uninstall"
        );
        assert!(registry.get(WEB_SEARCH_TOOL_NAME).is_none());
        assert!(
            registry
                .dynamic_names_for_source(WEB_SEARCH_SOURCE)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_key_name_falls_back_to_the_provider_default() {
        let _guard = EnvGuard::set("BRAVE_API_KEY", "brave-key");
        let cfg = enabled(WebSearchProvider::Brave);
        assert_eq!(
            resolve_api_key(&cfg, None).await.as_deref(),
            Some("brave-key")
        );
        // …and a keyless provider resolves nothing at all.
        assert!(
            resolve_api_key(&enabled(WebSearchProvider::Searxng), None)
                .await
                .is_none()
        );
    }
}
