//! Persistence for skill LLM-based risk assessments.
//!
//! A verdict is keyed on `(skill_name, content_hash)` so any edit to a
//! skill's directory produces a miss and forces a fresh LLM judgement.
//! Old verdicts for a given `skill_name` are intentionally kept so an
//! operator can see how the risk level has evolved across versions.
//!
//! Large skills go through a tiered flow: a primary-scope verdict on
//! `SKILL.md` alone is computed synchronously, and a full-scope job is
//! persisted in `skill_risk_assessment_jobs` so a restart mid-assessment
//! can resume instead of losing progress.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Coarse-grained risk classification returned by the assessor LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    /// No concern detected.
    Safe,
    /// Ambiguous or could be abused, but not inherently malicious.
    Suspicious,
    /// Prompt-injection, exfiltration, privilege-escalation, or
    /// destructive intent detected. Skill should be blocked from
    /// automatic use.
    Dangerous,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::Safe => "safe",
            RiskLevel::Suspicious => "suspicious",
            RiskLevel::Dangerous => "dangerous",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "safe" => Some(RiskLevel::Safe),
            "suspicious" => Some(RiskLevel::Suspicious),
            "dangerous" => Some(RiskLevel::Dangerous),
            _ => None,
        }
    }
}

/// Cached LLM verdict for a skill at a specific content hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskVerdict {
    pub skill_name: String,
    pub content_hash: String,
    pub level: RiskLevel,
    pub rationale: String,
    pub model: String,
    /// Unix epoch seconds.
    pub assessed_at: i64,
}

/// State of a background full-scope assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssessmentJobStatus {
    /// Queued but not yet picked up by a worker.
    Pending,
    /// A worker has claimed the job. If this state survives a restart,
    /// the worker died mid-flight and the job should be re-run.
    InProgress,
    /// Assessment finished but the verdict insert/job delete has not
    /// yet committed; treated the same as `Pending` on recovery.
    Failed,
}

impl AssessmentJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AssessmentJobStatus::Pending => "pending",
            AssessmentJobStatus::InProgress => "in_progress",
            AssessmentJobStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pending" => Some(AssessmentJobStatus::Pending),
            "in_progress" => Some(AssessmentJobStatus::InProgress),
            "failed" => Some(AssessmentJobStatus::Failed),
            _ => None,
        }
    }
}

/// Persisted row for an outstanding full-scope assessment.
///
/// `content_hash` is the full-directory hash captured when the job was
/// enqueued. If the directory has since changed, the worker re-hashes
/// on pickup and drops the stale row — a fresh `check` call will enqueue
/// a new job keyed on the new hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessmentJob {
    pub skill_name: String,
    pub content_hash: String,
    pub source_path: String,
    pub status: AssessmentJobStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    /// Unix epoch seconds.
    pub created_at: i64,
    /// Unix epoch seconds.
    pub updated_at: i64,
}

/// Persistence for skill risk verdicts and their background-job queue.
#[async_trait]
pub trait SkillRiskStore: Send + Sync {
    /// Look up the cached verdict for `(skill_name, content_hash)`.
    async fn get(&self, skill_name: &str, content_hash: &str) -> Result<Option<RiskVerdict>>;

    /// Insert or replace the verdict at `(skill_name, content_hash)`.
    async fn put(&self, verdict: &RiskVerdict) -> Result<()>;

    /// Remove every cached verdict for `skill_name`. Used when a skill
    /// is unregistered.
    async fn forget(&self, skill_name: &str) -> Result<()>;

    /// Insert or update a background assessment job row. Idempotent on
    /// `(skill_name, content_hash)` — enqueueing the same job twice is
    /// a no-op after the status fields are refreshed.
    async fn upsert_job(&self, job: &AssessmentJob) -> Result<()>;

    /// Atomically transition a job to a new status and refresh
    /// `updated_at` / `last_error` / `attempts`. Used by the worker to
    /// mark `InProgress` on pickup and `Failed` on giving up.
    async fn set_job_status(
        &self,
        skill_name: &str,
        content_hash: &str,
        status: AssessmentJobStatus,
        attempts: u32,
        last_error: Option<&str>,
    ) -> Result<()>;

    /// Remove a job row. Called after its verdict is persisted.
    async fn delete_job(&self, skill_name: &str, content_hash: &str) -> Result<()>;

    /// Load every job that is not yet final (`Pending`, `InProgress`,
    /// or `Failed` — the latter two imply prior work was interrupted and
    /// should be retried). Used at startup for crash recovery.
    async fn load_pending_jobs(&self) -> Result<Vec<AssessmentJob>>;
}
