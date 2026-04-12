use std::collections::HashMap;

use serde::{Deserialize, Serialize};
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

    /// Return every registered skill, sorted by name for stable operator output.
    pub fn all_sorted(&self) -> Vec<SkillDefinition> {
        let mut out: Vec<SkillDefinition> = self.skills.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Case-insensitive substring search across `name`, `description`, and
    /// command-trigger strings. An empty query matches every skill.
    pub fn search(&self, query: &str) -> Vec<SkillDefinition> {
        let needle = query.trim().to_ascii_lowercase();
        let mut hits: Vec<SkillDefinition> = self
            .skills
            .values()
            .filter(|s| {
                if needle.is_empty() {
                    return true;
                }
                if s.name.to_ascii_lowercase().contains(&needle)
                    || s.description.to_ascii_lowercase().contains(&needle)
                {
                    return true;
                }
                if let SkillTrigger::Command(cmd) = &s.trigger
                    && cmd.to_ascii_lowercase().contains(&needle)
                {
                    return true;
                }
                false
            })
            .cloned()
            .collect();
        hits.sort_by(|a, b| a.name.cmp(&b.name));
        hits
    }

    /// Validate every registered skill.
    ///
    /// Checks declarative shape (non-empty name/version/prompt) plus
    /// environment-level requirements declared in `SkillRequirements`:
    /// `required_bins` must resolve on `$PATH`, `required_env` must be set,
    /// `required_models` is reported as an informational note since the
    /// registry has no authoritative list of "known" models to reject
    /// against.
    pub fn validate_all(&self) -> Vec<SkillValidation> {
        let mut results: Vec<SkillValidation> = self.skills.values().map(validate_one).collect();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    /// Validate a single skill by name.
    pub fn validate(&self, name: &str) -> Option<SkillValidation> {
        self.skills.get(name).map(validate_one)
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

/// Outcome of a single skill's validation pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillValidation {
    pub name: String,
    pub ok: bool,
    pub issues: Vec<SkillIssue>,
    pub notes: Vec<String>,
}

/// One failing check on a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillIssue {
    pub kind: SkillIssueKind,
    pub detail: String,
}

/// Categorised issue kinds so operators can filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillIssueKind {
    EmptyName,
    EmptyVersion,
    EmptyPrompt,
    MissingBinary,
    MissingEnvVar,
}

fn validate_one(skill: &SkillDefinition) -> SkillValidation {
    let mut issues = Vec::new();
    let mut notes = Vec::new();

    if skill.name.trim().is_empty() {
        issues.push(SkillIssue {
            kind: SkillIssueKind::EmptyName,
            detail: "skill name is empty".into(),
        });
    }
    if skill.version.trim().is_empty() {
        issues.push(SkillIssue {
            kind: SkillIssueKind::EmptyVersion,
            detail: "skill version is empty".into(),
        });
    }
    if skill.prompt_template.trim().is_empty() {
        issues.push(SkillIssue {
            kind: SkillIssueKind::EmptyPrompt,
            detail: "prompt_template is empty".into(),
        });
    }

    for bin in &skill.requirements.required_bins {
        if !binary_on_path(bin) {
            issues.push(SkillIssue {
                kind: SkillIssueKind::MissingBinary,
                detail: format!("required binary '{bin}' not found on PATH"),
            });
        }
    }

    for var in &skill.requirements.required_env {
        if std::env::var(var).is_err() {
            issues.push(SkillIssue {
                kind: SkillIssueKind::MissingEnvVar,
                detail: format!("required env var '{var}' is not set"),
            });
        }
    }

    for model in &skill.requirements.required_models {
        notes.push(format!("declares required_models: {model}"));
    }

    SkillValidation {
        name: skill.name.clone(),
        ok: issues.is_empty(),
        issues,
        notes,
    }
}

fn binary_on_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PostProcessing, SkillDefinition, SkillRequirements};
    use aura_registry::{ArtifactSource, TrustLevel};

    fn mk(name: &str, description: &str, trigger: SkillTrigger) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            version: "0.1.0".into(),
            description: description.into(),
            trigger,
            prompt_template: "be helpful".into(),
            allowed_tools: vec![],
            post_processing: Some(PostProcessing {
                output_template: None,
                summarize: false,
            }),
            source: ArtifactSource::Workspace,
            trust_level: TrustLevel::Trusted,
            requirements: SkillRequirements::default(),
            token_budget_hint: 1024,
        }
    }

    #[test]
    fn search_filters_by_name_description_and_command() {
        let mut reg = SkillRegistry::new();
        reg.register(mk(
            "summarize",
            "condense long text",
            SkillTrigger::Command("/summarize".into()),
        ));
        reg.register(mk(
            "translate",
            "convert between languages",
            SkillTrigger::AgentDecision,
        ));
        reg.register(mk(
            "codegen",
            "generate helper code snippets",
            SkillTrigger::AgentDecision,
        ));

        // substring in name
        let hits = reg.search("codegen");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "codegen");

        // substring in description
        let hits = reg.search("LANGUAGES");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "translate");

        // substring in trigger command
        let hits = reg.search("/summ");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "summarize");

        // empty query returns all
        let hits = reg.search("");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn validate_all_reports_ok_for_minimal_skill() {
        let mut reg = SkillRegistry::new();
        reg.register(mk("hello", "greet the user", SkillTrigger::AgentDecision));
        let reports = reg.validate_all();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].ok);
        assert!(reports[0].issues.is_empty());
    }

    #[test]
    fn validate_flags_missing_binary_and_env_var() {
        let mut skill = mk(
            "needs-deps",
            "calls external tool",
            SkillTrigger::AgentDecision,
        );
        skill.requirements.required_bins = vec!["definitely_not_a_real_binary_12345".into()];
        skill.requirements.required_env = vec!["AURA_NONEXISTENT_ENV_VAR_FOR_TESTS".into()];

        let mut reg = SkillRegistry::new();
        reg.register(skill);
        let report = reg.validate("needs-deps").expect("skill exists");
        assert!(!report.ok);
        assert_eq!(report.issues.len(), 2);
        let kinds: Vec<_> = report.issues.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&SkillIssueKind::MissingBinary));
        assert!(kinds.contains(&SkillIssueKind::MissingEnvVar));
    }

    #[test]
    fn validate_flags_empty_prompt() {
        let mut skill = mk("blank", "has no prompt", SkillTrigger::AgentDecision);
        skill.prompt_template = "   ".into();
        let mut reg = SkillRegistry::new();
        reg.register(skill);
        let report = reg.validate("blank").expect("skill exists");
        assert!(!report.ok);
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.kind == SkillIssueKind::EmptyPrompt)
        );
    }

    #[test]
    fn validate_single_missing_skill_returns_none() {
        let reg = SkillRegistry::new();
        assert!(reg.validate("ghost").is_none());
    }
}
