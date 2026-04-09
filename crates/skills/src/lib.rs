use aura_registry::{ArtifactSource, TrustLevel};
use serde::{Deserialize, Serialize};

/// A declarative skill definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub name: String,
    pub version: String,
    pub description: String,
    pub trigger: SkillTrigger,
    pub prompt_template: String,
    pub allowed_tools: Vec<String>,
    pub post_processing: Option<PostProcessing>,
    pub source: ArtifactSource,
    pub trust_level: TrustLevel,
    pub requirements: SkillRequirements,
    pub token_budget_hint: usize,
}

/// How a skill is triggered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkillTrigger {
    /// Triggered by an exact command string (e.g., "/summarize").
    Command(String),
    /// Triggered by a regex pattern match on the message.
    #[serde(with = "regex_serde")]
    Pattern(regex::Regex),
    /// Triggered by the agent's autonomous decision.
    AgentDecision,
}

/// Serde helper for `regex::Regex`.
mod regex_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(re: &regex::Regex, s: S) -> Result<S::Ok, S::Error> {
        re.as_str().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<regex::Regex, D::Error> {
        let pattern = String::deserialize(d)?;
        regex::Regex::new(&pattern).map_err(serde::de::Error::custom)
    }
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

/// Optional post-processing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostProcessing {
    pub output_template: Option<String>,
    pub summarize: bool,
}
