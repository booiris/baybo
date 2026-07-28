use std::collections::HashMap;

use baybo_model::{LlmEntryName, ModelTier};
use serde::{Deserialize, Serialize};

/// Top-level agent configuration: execution policy and context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// Maximum LLM iterations per user message before the loop stops.
    pub max_iterations: usize,
    /// Context window configuration.
    pub context: ContextConfig,
    /// Recursion cap for `spawn_subagent`. A parent session at depth
    /// `>= max_subagent_depth` is rejected with
    /// `ToolError::SubagentDepthExceeded` rather than spawning. Depth
    /// 0 = top-level session, depth N = subagent N levels deep.
    pub max_subagent_depth: u32,
    /// Horizontal fan-out cap. The fan-out limiter rejects
    /// `spawn_subagent` once `max_subagents_per_root` subagents
    /// (foreground + background) are already running under the same
    /// root session, surfacing as
    /// `ToolError::SubagentFanoutExceeded`. Independent of depth.
    pub max_subagents_per_root: u32,
    /// Maps coarse `ModelTier` (Lite / Balanced / Deep) to a concrete
    /// `llm[*].name`. Two consumers:
    ///
    /// - `spawn_subagent`'s tier resolution, when neither the call's
    ///   explicit `llm` override nor the profile's default supplies a
    ///   name. Unmapped tiers fall through to the pool's `default-llm`
    ///   with a `warn!` so operator misconfig is visible.
    /// - `LlmClientPool::resolve_lite`, which uses the **`Lite`** entry as
    ///   the fallback for auxiliary LLM calls (the Bash risk judges,
    ///   WebFetch's page summary, title generation) when the resolved
    ///   entry declares no `lite_model` of its own.
    ///
    /// So re-pointing `lite` moves both the cheap subagent tier and the
    /// auxiliary calls. An operator who wants them apart sets a per-entry
    /// `lite_model`, which outranks this map.
    ///
    /// The `fast` key is the pre-rename spelling of `lite` and still
    /// deserializes — these are enum-typed map keys, so dropping the alias
    /// would turn an older `baybo.json` into a load failure.
    pub model_tiers: HashMap<ModelTier, LlmEntryName>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 1000,
            context: ContextConfig::default(),
            max_subagent_depth: 3,
            max_subagents_per_root: 8,
            model_tiers: HashMap::new(),
        }
    }
}

/// Context window budget and compression settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ContextConfig {
    /// Fraction of the active model's context window at which
    /// compression triggers. Must be in `(0.0, 1.0]`.
    pub compression_threshold: f64,
    /// Number of most recent messages to retain when truncating.
    pub keep_recent: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 0.65,
            keep_recent: 10,
        }
    }
}
