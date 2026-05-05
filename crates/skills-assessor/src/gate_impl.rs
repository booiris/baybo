//! Bridges the inherent [`SkillAssessor::check`] (`Result<AssessedSkill, _>`)
//! onto the trait used by tool-side consumers. Trait lives in
//! `aura-skills` so neither side needs to depend on the other for
//! the gate decision.

use async_trait::async_trait;
use aura_skills::{SkillDefinition, SkillGate, SkillRiskCheck};
use tracing::warn;

use crate::{AssessError, RiskLevel, SkillAssessor};

#[async_trait]
impl SkillRiskCheck for SkillAssessor {
    async fn assess(&self, skill: &SkillDefinition) -> SkillGate {
        match SkillAssessor::check(self, skill).await {
            Ok(assessed) => match assessed.verdict.level {
                RiskLevel::Dangerous => {
                    warn!(
                        skill = %skill.name,
                        scope = %assessed.scope.as_str(),
                        rationale = %assessed.verdict.rationale,
                        "skill blocked by risk assessor",
                    );
                    SkillGate::Block {
                        rationale: assessed.verdict.rationale,
                    }
                }
                RiskLevel::Suspicious => {
                    warn!(
                        skill = %skill.name,
                        scope = %assessed.scope.as_str(),
                        background_pending = assessed.background_pending,
                        rationale = %assessed.verdict.rationale,
                        "skill rated suspicious — invoking with warning",
                    );
                    SkillGate::PassWithWarning {
                        rationale: assessed.verdict.rationale,
                    }
                }
                RiskLevel::Safe => SkillGate::Pass,
            },
            Err(AssessError::NoSourcePath) => SkillGate::Pass,
            Err(err) => {
                warn!(
                    skill = %skill.name,
                    error = %err,
                    "risk assessor failed; allowing skill through",
                );
                SkillGate::Pass
            }
        }
    }
}
