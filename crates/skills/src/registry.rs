use std::collections::HashMap;
use std::sync::Arc;

use aura_core::{Message, TrustLevel};
use tokio::sync::RwLock;

use crate::{
    SkillCandidate, SkillDefinition, SkillSelectionContext, requirements_met, score_skill,
};

/// Configuration for the skill selector.
#[derive(Debug, Clone)]
pub struct SkillSelector {
    /// Maximum total prompt tokens for injected skill descriptions.
    pub max_prompt_tokens: usize,
    /// Maximum number of tools an `Installed`-trust skill may request.
    pub max_tools_for_installed: usize,
}

impl Default for SkillSelector {
    fn default() -> Self {
        Self {
            max_prompt_tokens: 4096,
            max_tools_for_installed: 5,
        }
    }
}

/// Registry that holds loaded skill definitions and provides selection.
pub struct SkillRegistry {
    skills: Arc<RwLock<HashMap<String, SkillDefinition>>>,
    selector: SkillSelector,
}

impl SkillRegistry {
    pub fn new(selector: SkillSelector) -> Self {
        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
            selector,
        }
    }

    /// Registers a skill definition. Overwrites any existing skill with the same name.
    pub async fn register(&self, skill: SkillDefinition) {
        let mut skills = self.skills.write().await;
        skills.insert(skill.name.clone(), skill);
    }

    /// Loads a skill from a single JSON file.
    pub async fn load_from_file(&self, path: &std::path::Path) -> aura_core::Result<()> {
        let skill = crate::loader::load_from_file(path).await?;
        self.register(skill).await;
        Ok(())
    }

    /// Loads all skill JSON files from a directory.
    /// Returns errors for individual files that failed to load.
    pub async fn load_from_dir(
        &self,
        dir: &std::path::Path,
    ) -> aura_core::Result<Vec<aura_core::AuraError>> {
        let (skills, errors) = crate::loader::load_from_dir(dir).await?;
        let mut guard = self.skills.write().await;
        for skill in skills {
            guard.insert(skill.name.clone(), skill);
        }
        Ok(errors)
    }

    /// Retrieves a skill by name.
    pub async fn get(&self, name: &str) -> Option<SkillDefinition> {
        let skills = self.skills.read().await;
        skills.get(name).cloned()
    }

    /// Lists all registered skill names.
    pub async fn list(&self) -> Vec<String> {
        let skills = self.skills.read().await;
        skills.keys().cloned().collect()
    }

    /// Selects matching skills for a message given the current context.
    ///
    /// Pipeline: gating -> scoring -> token budget -> tool ceiling attenuation -> final selection.
    pub async fn select(
        &self,
        input: &Message,
        ctx: &SkillSelectionContext,
    ) -> Vec<SkillCandidate> {
        let skills = self.skills.read().await;

        let mut candidates: Vec<SkillCandidate> = skills
            .values()
            // Gating: filter out skills with unmet requirements or untrusted
            .filter(|s| {
                if s.trust_level == TrustLevel::Untrusted {
                    return false;
                }
                requirements_met(&s.requirements, ctx)
            })
            // Scoring
            .filter_map(|s| {
                let score = score_skill(s, input);
                if score > 0.0 {
                    Some(SkillCandidate {
                        skill: s.clone(),
                        score,
                    })
                } else {
                    None
                }
            })
            .collect();

        // Sort by score descending
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Token budget: accumulate until budget is exhausted
        let mut total_tokens = 0usize;
        candidates.retain(|c| {
            if total_tokens + c.skill.token_budget_hint <= ctx.available_tokens {
                total_tokens += c.skill.token_budget_hint;
                true
            } else {
                false
            }
        });

        // Tool ceiling attenuation for Installed skills
        for candidate in &mut candidates {
            if candidate.skill.trust_level == TrustLevel::Installed
                && candidate.skill.allowed_tools.len() > self.selector.max_tools_for_installed
            {
                candidate
                    .skill
                    .allowed_tools
                    .truncate(self.selector.max_tools_for_installed);
            }
        }

        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SkillRequirements, SkillTrigger};
    use aura_core::{ArtifactSource, ChannelType, ContentBlock, MessageMetadata, User};

    fn make_message(content: &str) -> Message {
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

    fn make_skill(name: &str, trigger: SkillTrigger, trust: TrustLevel) -> SkillDefinition {
        SkillDefinition {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            description: format!("Skill {name}"),
            trigger,
            prompt_template: "{{input}}".to_string(),
            allowed_tools: vec!["tool_a".to_string(), "tool_b".to_string()],
            post_processing: None,
            source: ArtifactSource::Workspace,
            trust_level: trust,
            requirements: SkillRequirements::default(),
            token_budget_hint: 100,
        }
    }

    fn default_ctx() -> SkillSelectionContext {
        SkillSelectionContext {
            available_tokens: 1000,
            available_env: vec![],
            available_bins: vec![],
            available_models: vec![],
        }
    }

    #[tokio::test]
    async fn test_select_command_match() {
        let reg = SkillRegistry::new(SkillSelector::default());
        reg.register(make_skill(
            "summarize",
            SkillTrigger::Command("/summarize".to_string()),
            TrustLevel::Trusted,
        ))
        .await;

        let msg = make_message("/summarize this");
        let candidates = reg.select(&msg, &default_ctx()).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].skill.name, "summarize");
    }

    #[tokio::test]
    async fn test_select_filters_untrusted() {
        let reg = SkillRegistry::new(SkillSelector::default());
        reg.register(make_skill(
            "untrusted-skill",
            SkillTrigger::AgentDecision,
            TrustLevel::Untrusted,
        ))
        .await;

        let msg = make_message("anything");
        let candidates = reg.select(&msg, &default_ctx()).await;
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_select_respects_token_budget() {
        let reg = SkillRegistry::new(SkillSelector::default());
        // Each skill needs 600 tokens, budget is 1000 => only 1 fits
        let mut s1 = make_skill("s1", SkillTrigger::AgentDecision, TrustLevel::Trusted);
        s1.token_budget_hint = 600;
        let mut s2 = make_skill("s2", SkillTrigger::AgentDecision, TrustLevel::Trusted);
        s2.token_budget_hint = 600;

        reg.register(s1).await;
        reg.register(s2).await;

        let msg = make_message("do something");
        let candidates = reg.select(&msg, &default_ctx()).await;
        assert_eq!(candidates.len(), 1);
    }

    #[tokio::test]
    async fn test_select_filters_unmet_requirements() {
        let reg = SkillRegistry::new(SkillSelector::default());
        let mut skill = make_skill(
            "docker-skill",
            SkillTrigger::AgentDecision,
            TrustLevel::Trusted,
        );
        skill.requirements.required_bins = vec!["docker".to_string()];
        reg.register(skill).await;

        let msg = make_message("anything");
        // No docker in available_bins
        let candidates = reg.select(&msg, &default_ctx()).await;
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_tool_ceiling_attenuation() {
        let reg = SkillRegistry::new(SkillSelector {
            max_prompt_tokens: 4096,
            max_tools_for_installed: 1,
        });
        let mut skill = make_skill(
            "installed-skill",
            SkillTrigger::AgentDecision,
            TrustLevel::Installed,
        );
        skill.allowed_tools = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        reg.register(skill).await;

        let msg = make_message("anything");
        let candidates = reg.select(&msg, &default_ctx()).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].skill.allowed_tools.len(), 1);
    }
}
