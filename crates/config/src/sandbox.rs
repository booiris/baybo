use serde::{Deserialize, Serialize};

/// Sandbox execution and network policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SandboxConfig {
    pub wasm: WasmLimitsConfig,
    pub network: NetworkPolicyConfig,
}

/// Resource limits for WASM module execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WasmLimitsConfig {
    /// Execution timeout in milliseconds.
    pub timeout_ms: u64,
    /// Maximum memory in bytes.
    pub max_memory_bytes: usize,
    /// Maximum fuel (instruction budget).
    pub max_fuel: u64,
}

impl Default for WasmLimitsConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_memory_bytes: 64 * 1024 * 1024,
            max_fuel: 1_000_000_000,
        }
    }
}

/// Network access policy for sandboxed code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct NetworkPolicyConfig {
    /// Explicitly allowed domains.
    pub allowed_domains: Vec<String>,
    /// Whether loopback access is permitted.
    pub allow_loopback: bool,
}
