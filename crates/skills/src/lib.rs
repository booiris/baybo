pub mod loader;
pub mod registry;
pub mod render;
pub mod validation;

pub use registry::{SkillCandidate, SkillIssue, SkillIssueKind, SkillRegistry, SkillValidation};

use aura_registry::{ArtifactSource, TrustLevel};
use serde::{Deserialize, Serialize};

/// A declarative skill definition.
///
/// Every skill may be invoked both as a `/<command>` slash command and by
/// the model based on its `description`. The two entry points are toggled
/// independently by `command` and `agent_invocable`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Slash-command name (no leading `/`) that invokes this skill.
    /// `None` when the SKILL.md sets `user-invocable: false`.
    pub command: Option<String>,
    /// Whether the model may auto-select this skill from its description.
    /// `false` when the SKILL.md sets `disable-model-invocation: true`.
    pub agent_invocable: bool,
    /// Autocomplete hint for the slash command (e.g., `[issue-number]`).
    pub argument_hint: Option<String>,
    pub prompt_template: String,
    pub allowed_tools: Vec<String>,
    pub source: ArtifactSource,
    pub trust_level: TrustLevel,
    pub requirements: SkillRequirements,
    pub token_budget_hint: usize,
    /// On-disk directory the skill was loaded from, when known. Used by
    /// the out-of-process risk assessor to hash the full skill tree (not
    /// just the parsed prompt body) so supporting files like helper
    /// scripts also invalidate the cached verdict on change.
    #[serde(default)]
    pub source_path: Option<std::path::PathBuf>,
}

/// Pre-execution requirements that must be satisfied.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillRequirements {
    #[serde(default)]
    pub required_bins: Vec<String>,
    #[serde(default)]
    pub required_env: Vec<String>,
    #[serde(default)]
    pub required_models: Vec<String>,
}
