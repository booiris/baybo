//! Trait-based risk-check hook. Lives here rather than in
//! `baybo-skills-assessor` to dodge a circular dep — the assessor
//! crate already depends on `baybo-skills` for `SkillDefinition`.

use async_trait::async_trait;

use crate::SkillDefinition;

/// Outcome of running a risk check against one skill.
#[derive(Debug, Clone)]
pub enum SkillGate {
    Pass,
    PassWithWarning { rationale: String },
    Block { rationale: String },
}

/// Object-safe risk check.
///
/// Transient errors must fall through as `Pass`: a flaky judge
/// should not gate the agent (`docs/modules/skills.md` makes
/// availability the design preference over false-positive blocks).
#[async_trait]
pub trait SkillRiskCheck: Send + Sync {
    async fn assess(&self, skill: &SkillDefinition) -> SkillGate;
}

/// Always-`Pass` no-op for tests and call sites that opt out of
/// risk gating.
pub struct AlwaysPass;

#[async_trait]
impl SkillRiskCheck for AlwaysPass {
    async fn assess(&self, _skill: &SkillDefinition) -> SkillGate {
        SkillGate::Pass
    }
}
