use std::collections::HashMap;

use tracing::debug;

use crate::{SkillDefinition, SkillTrigger};

/// A matched skill with its relevance score.
#[derive(Debug, Clone)]
pub struct SkillCandidate {
    pub skill: SkillDefinition,
    pub score: f64,
}

/// Central registry for skill definitions.
///
/// Skills are loaded from workspace files or the extension registry.
/// The `select` method finds skills that match a given user message
/// via command prefix, regex pattern, or agent-decision triggers.
pub struct SkillRegistry {
    skills: HashMap<String, SkillDefinition>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Register a skill definition.
    pub fn register(&mut self, skill: SkillDefinition) {
        debug!(name = %skill.name, "registering skill");
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Remove a skill by name.
    pub fn remove(&mut self, name: &str) -> Option<SkillDefinition> {
        self.skills.remove(name)
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&SkillDefinition> {
        self.skills.get(name)
    }

    /// List all registered skill names.
    pub fn list(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    /// Select skills that match the given user message text.
    ///
    /// Selection pipeline:
    /// 1. **Command match**: exact `/command` prefix → score 1.0
    /// 2. **Pattern match**: regex match on message → score 0.8
    /// 3. **AgentDecision**: always eligible → score 0.5
    ///
    /// Results are sorted by score descending.
    pub fn select(&self, message_text: &str) -> Vec<SkillCandidate> {
        let mut candidates = Vec::new();

        for skill in self.skills.values() {
            let score = match &skill.trigger {
                SkillTrigger::Command(cmd) => {
                    if message_text.starts_with(cmd.as_str()) {
                        1.0
                    } else {
                        continue;
                    }
                }
                SkillTrigger::Pattern(re) => {
                    if re.is_match(message_text) {
                        0.8
                    } else {
                        continue;
                    }
                }
                SkillTrigger::AgentDecision => 0.5,
            };

            candidates.push(SkillCandidate {
                skill: skill.clone(),
                score,
            });
        }

        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        candidates
    }
}
