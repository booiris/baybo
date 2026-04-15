//! LLM-backed risk assessment for skill directories.
//!
//! A skill's `SKILL.md` and any supporting scripts are untrusted input
//! to the assistant. Before a skill is used, the assessor asks an LLM
//! to classify it as `Safe`, `Suspicious`, or `Dangerous`, caching the
//! verdict under a content hash. The cache is invalidated automatically
//! when the skill tree changes, so tampered skills are re-judged.
//!
//! ```text
//!   +-----------+   hash(dir)    +-----------------+   miss    +------------+
//!   |  caller   | -------------> |  SkillRiskStore::get | --------> |  LlmClient |
//!   +-----------+                +-----------------+           +------------+
//!         ^                             |                             |
//!         |         hit (cached)        |        put verdict          |
//!         +-----------------------------+-----------------------------+
//! ```
//!
//! The assessor deliberately does not own an in-process regex or
//! heuristic ruleset — the value here is the LLM's ability to read the
//! prompt body and flag intent. The deterministic `aura_skills::validation`
//! layer still handles structural safety (name/version grammar,
//! `<skill>` tag injection).

mod assessor;
mod hash;
mod prompt;
mod queue;

pub use assessor::{AssessError, AssessedSkill, AssessmentMode, AssessmentScope, SkillAssessor};
pub use aura_storage::{
    AssessmentJob, AssessmentJobStatus, RiskLevel, RiskVerdict, SkillRiskStore,
};
pub use hash::{hash_skill_dir, hash_skill_primary};
