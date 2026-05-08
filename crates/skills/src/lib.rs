pub mod gate;
pub mod linked_files;
pub mod loader;
pub mod registry;
pub mod render;
pub mod tools;
pub mod validation;

pub use gate::{AlwaysPass, SkillGate, SkillRiskCheck};
pub use linked_files::LinkedFiles;
pub use registry::{
    SkillCandidate, SkillIssue, SkillIssueKind, SkillRegistry, SkillSummary, SkillValidation,
};
pub use tools::{SKILL_INPUT_NAME_FIELD, SKILL_TOOL_NAME, build as build_skill_tool};

/// Directory-level hard-reject thresholds shared between the
/// risk-assessor (which hashes the tree before judging) and the
/// `Skill` tool (which enumerates linked files for Mode 1 output).
/// Both must agree so a tree the assessor refuses can't be loaded
/// through the tool, and vice versa.
pub const MAX_SKILL_DIR_FILES: usize = 500;
pub const MAX_SKILL_DIR_BYTES: u64 = 100 * 1024 * 1024;

use aura_model::{ArtifactSource, TrustLevel};
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
    /// Categorized helper-file inventory cached from `source_path` at
    /// load time. Empty for inline-defined skills with no on-disk
    /// directory; refreshed by [`SkillRegistry::reload`].
    #[serde(default)]
    pub linked_files: LinkedFiles,
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
