//! Runtime construction of the pluggable memory backend.
//!
//! The bootstrapping logic lives next to the trait + implementations so
//! that adding a new backend is a single-crate change — `src/runtime.rs`
//! just plumbs `(config.memory, vault, proxy)` in and wires the resulting
//! handle into the actor graph.

use std::sync::Arc;

use aura_config::{AuraConfig, MemoryProvider};
use aura_security::SecretVault;
use aura_security::http::ProxySettings;

use crate::Memory;
use crate::backends::{mem0, openviking};

/// Build the pluggable memory backend selected by
/// `config.memory.provider`, run its startup probe, and return the
/// handle wrapped as `Arc<dyn Memory>`.
///
/// Returns `None` when:
///
/// - `memory.enabled = false`,
/// - `provider = noop`,
/// - config parse fails (the backend's `parse_extra` errors), or
/// - a required API key is missing (mem0 only — openviking is optional).
///
/// All `None` paths log a `warn!` instead of returning an error: the rest
/// of aura should still come up, so the operator can fix the credential
/// and restart. The returned handle's `tools()` should be registered into
/// the builtin tool registry by the caller (see `src/runtime.rs`); we do
/// not do it here so the function stays free of any mutable-registry
/// argument.
pub async fn build_memory_backend(
    config: &AuraConfig,
    vault: &Arc<SecretVault>,
    proxy: Option<&ProxySettings>,
) -> Option<Arc<dyn Memory>> {
    if !config.memory.enabled {
        return None;
    }
    let memory: Arc<dyn Memory> = match config.memory.provider {
        MemoryProvider::Noop => return None,
        MemoryProvider::Mem0 => build_mem0(config, vault, proxy).await?,
        MemoryProvider::OpenViking => build_openviking(config, vault, proxy).await?,
    };
    Some(memory)
}

async fn build_mem0(
    config: &AuraConfig,
    vault: &Arc<SecretVault>,
    proxy: Option<&ProxySettings>,
) -> Option<Arc<dyn Memory>> {
    let cfg = match mem0::parse_extra(&config.memory.extra) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "mem0 config parse failed; memory disabled");
            return None;
        }
    };
    let key = match mem0::resolve_api_key(&cfg, Some(vault.as_ref())).await {
        Some(k) if !k.is_empty() => k,
        _ => {
            tracing::warn!(
                "mem0 API key not found; memory disabled. Run \
                 `aura secret add MEM0_API_KEY` or set the MEM0_API_KEY env var."
            );
            return None;
        }
    };
    match mem0::Mem0Memory::new(cfg, key, proxy) {
        Ok(m) => {
            m.probe().await;
            Some(Arc::new(m))
        }
        Err(e) => {
            tracing::warn!(error = %e, "mem0 backend construction failed; memory disabled");
            None
        }
    }
}

async fn build_openviking(
    config: &AuraConfig,
    vault: &Arc<SecretVault>,
    proxy: Option<&ProxySettings>,
) -> Option<Arc<dyn Memory>> {
    let cfg = match openviking::parse_extra(&config.memory.extra) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "openviking config parse failed; memory disabled");
            return None;
        }
    };
    let key = openviking::resolve_api_key(&cfg, Some(vault.as_ref())).await;
    match openviking::OpenVikingMemory::new(cfg, key, proxy) {
        Ok(m) => {
            m.probe().await;
            Some(Arc::new(m))
        }
        Err(e) => {
            tracing::warn!(error = %e, "openviking construction failed; memory disabled");
            None
        }
    }
}
