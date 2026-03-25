pub mod loader;
pub mod registry;

use aura_core::{ArtifactSource, Message, TrustLevel};
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
    /// Required local binaries (e.g., "git", "docker").
    #[serde(default)]
    pub required_bins: Vec<String>,
    /// Required environment variables.
    #[serde(default)]
    pub required_env: Vec<String>,
    /// Required model capabilities.
    #[serde(default)]
    pub required_models: Vec<String>,
}

/// Optional post-processing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostProcessing {
    /// Template for formatting the output.
    pub output_template: Option<String>,
    /// Whether to summarize long outputs.
    pub summarize: bool,
}

/// A skill candidate returned by the selection pipeline.
#[derive(Debug, Clone)]
pub struct SkillCandidate {
    pub skill: SkillDefinition,
    pub score: f64,
}

/// Context provided to the skill selection pipeline.
#[derive(Debug, Clone)]
pub struct SkillSelectionContext {
    /// Maximum total prompt tokens available for skill injection.
    pub available_tokens: usize,
    /// Environment variables currently set.
    pub available_env: Vec<String>,
    /// Binaries available on the system.
    pub available_bins: Vec<String>,
    /// Models available in the current configuration.
    pub available_models: Vec<String>,
}

/// Checks whether a skill's requirements are satisfied by the given context.
pub fn requirements_met(req: &SkillRequirements, ctx: &SkillSelectionContext) -> bool {
    let bins_ok = req
        .required_bins
        .iter()
        .all(|b| ctx.available_bins.contains(b));
    let env_ok = req
        .required_env
        .iter()
        .all(|e| ctx.available_env.contains(e));
    let models_ok = req
        .required_models
        .iter()
        .all(|m| ctx.available_models.contains(m));
    bins_ok && env_ok && models_ok
}

/// Extracts the concatenated text content from a message's content blocks.
pub fn extract_text(message: &Message) -> String {
    use aura_core::ContentBlock;
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Scores a skill against an incoming message.
/// Returns 0.0 if the trigger does not match.
pub fn score_skill(skill: &SkillDefinition, message: &Message) -> f64 {
    let text = extract_text(message);
    match &skill.trigger {
        SkillTrigger::Command(cmd) => {
            if text.starts_with(cmd.as_str()) {
                1.0
            } else {
                0.0
            }
        }
        SkillTrigger::Pattern(re) => {
            if re.is_match(&text) {
                0.8
            } else {
                0.0
            }
        }
        SkillTrigger::AgentDecision => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(content: &str) -> Message {
        use aura_core::{ChannelType, ContentBlock, MessageMetadata, User};
        Message {
            id: "test".to_string(),
            session_id: "s1".to_string(),
            channel: ChannelType::Cli,
            sender: User {
                id: "u1".to_string(),
                name: None,
                channel: ChannelType::Cli,
            },
            content: vec![ContentBlock::Text(content.to_string())],
            timestamp: chrono::Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        }
    }

    fn make_skill(trigger: SkillTrigger) -> SkillDefinition {
        SkillDefinition {
            name: "test-skill".to_string(),
            version: "1.0.0".to_string(),
            description: "A test skill".to_string(),
            trigger,
            prompt_template: "Do something with {{input}}".to_string(),
            allowed_tools: vec![],
            post_processing: None,
            source: ArtifactSource::Workspace,
            trust_level: TrustLevel::Trusted,
            requirements: SkillRequirements::default(),
            token_budget_hint: 100,
        }
    }

    #[test]
    fn test_command_trigger_match() {
        let skill = make_skill(SkillTrigger::Command("/summarize".to_string()));
        let msg = make_message("/summarize this text");
        assert_eq!(score_skill(&skill, &msg), 1.0);
    }

    #[test]
    fn test_command_trigger_no_match() {
        let skill = make_skill(SkillTrigger::Command("/summarize".to_string()));
        let msg = make_message("hello world");
        assert_eq!(score_skill(&skill, &msg), 0.0);
    }

    #[test]
    fn test_pattern_trigger_match() {
        let re = regex::Regex::new(r"(?i)summarize").unwrap();
        let skill = make_skill(SkillTrigger::Pattern(re));
        let msg = make_message("Please summarize this");
        assert_eq!(score_skill(&skill, &msg), 0.8);
    }

    #[test]
    fn test_agent_decision_trigger() {
        let skill = make_skill(SkillTrigger::AgentDecision);
        let msg = make_message("anything");
        assert_eq!(score_skill(&skill, &msg), 0.5);
    }

    #[test]
    fn test_requirements_met_all_satisfied() {
        let req = SkillRequirements {
            required_bins: vec!["git".to_string()],
            required_env: vec!["HOME".to_string()],
            required_models: vec![],
        };
        let ctx = SkillSelectionContext {
            available_tokens: 1000,
            available_env: vec!["HOME".to_string(), "PATH".to_string()],
            available_bins: vec!["git".to_string(), "curl".to_string()],
            available_models: vec![],
        };
        assert!(requirements_met(&req, &ctx));
    }

    #[test]
    fn test_requirements_not_met() {
        let req = SkillRequirements {
            required_bins: vec!["docker".to_string()],
            required_env: vec![],
            required_models: vec![],
        };
        let ctx = SkillSelectionContext {
            available_tokens: 1000,
            available_env: vec![],
            available_bins: vec!["git".to_string()],
            available_models: vec![],
        };
        assert!(!requirements_met(&req, &ctx));
    }
}
