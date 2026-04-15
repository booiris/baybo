use serde::{Deserialize, Serialize};

/// Skill-related runtime configuration.
///
/// Currently only covers the LLM-backed risk assessor knob. Other skill
/// concerns (hot reload, extra directories, trust tiers) are still
/// sourced from defaults; see `docs/modules/config.md` §"Out-of-scope
/// modules" for the phasing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SkillsConfig {
    pub risk_check: RiskCheckConfig,
}

/// How aggressively the skill risk assessor should classify a skill
/// before it is made available to the agent.
///
/// Mirrored into `aura_skills_assessor::AssessmentMode` at bootstrap.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskCheckConfig {
    /// Skip the LLM classifier. Every skill is treated as Safe.
    Off,
    /// Classify only `SKILL.md`. Helper files are not read or judged.
    /// Default — cheap enough to run on every first use without
    /// blocking the caller, and covers the prompt-injection threat
    /// model that motivates the assessor in the first place.
    #[default]
    Primary,
    /// Classify the full directory tree synchronously on first use.
    /// Slower but covers helper scripts as well as the prompt body.
    Full,
}
