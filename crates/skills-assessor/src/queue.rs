//! Background worker for full-scope skill risk assessments.
//!
//! The worker runs on its own Tokio task and drains an mpsc queue
//! serially — one LLM call at a time, to keep provider load bounded.
//! Under `AssessmentMode::Full`, large skills (> `TIER_MAX_FILES` or
//! `TIER_MAX_BYTES`) are tiered: `check_full` classifies `SKILL.md`
//! synchronously and enqueues a full-scope job here so a chat turn
//! doesn't block on a big LLM prompt. The worker also drains any
//! `skill_risk_assessment_jobs` rows left behind by older binaries so
//! upgrades don't silently abandon in-flight verdicts.
//!
//! "Progress" in this system is coarse: an LLM call is atomic, so the
//! only resumable state is "the job is still owed, run it again." If a
//! skill's content changed while the job was queued the worker drops
//! the stale job and a fresh `check` call will produce a new verdict.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aura_llm::{BoundBilledLlm, ChatRequest};
use aura_skills::SkillDefinition;
use aura_store::{AssessmentJob, AssessmentJobStatus, RiskLevel, RiskVerdict, SkillRiskStore};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::hash::hash_skill_dir;
use crate::prompt::{Scope, build_messages, parse_verdict};

/// Full-scope work order. Produced by `check_full` when a skill trips
/// the tier threshold and at startup when recovering rows from the
/// persisted jobs table. A copy of the skill definition is captured so
/// the worker can reconstruct the prompt without re-locating the skill
/// in the registry (which may have been reloaded by the time the job
/// runs).
#[derive(Debug, Clone)]
pub struct BackgroundJob {
    pub skill: SkillDefinition,
    pub source_path: PathBuf,
    /// Full-dir hash at enqueue time. Used as the cache key and as the
    /// staleness check when the worker picks the job up.
    pub expected_hash: String,
}

/// Maximum retries before a job is marked `Failed` and left in place
/// for operator inspection. Kept small because an LLM that reliably
/// errors is more likely provider-side than transient.
const MAX_ATTEMPTS: u32 = 3;

/// Delay between retries. Simple fixed backoff — the worker is already
/// serial so we don't need per-job exponential jitter.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Spawn the background worker task.
///
/// Returns an mpsc sender the assessor clones into every `check` call.
/// Dropping all senders shuts the worker down after draining.
pub(crate) fn spawn_worker(
    llm: Arc<BoundBilledLlm>,
    store: Arc<dyn SkillRiskStore>,
    capacity: usize,
) -> mpsc::Sender<BackgroundJob> {
    let (tx, mut rx) = mpsc::channel::<BackgroundJob>(capacity);
    tokio::spawn(async move {
        debug!("skill-risk background worker started");
        while let Some(job) = rx.recv().await {
            process_job(&llm, store.as_ref(), job).await;
        }
        debug!("skill-risk background worker stopped (all senders dropped)");
    });
    tx
}

async fn process_job(llm: &BoundBilledLlm, store: &dyn SkillRiskStore, job: BackgroundJob) {
    let BackgroundJob {
        skill,
        source_path,
        expected_hash,
    } = job;

    // Re-hash on pickup. If the skill directory has changed since the
    // job was enqueued, the expected hash is stale and a fresh `check`
    // will enqueue a new job keyed on the new hash. Drop the stale row
    // and move on.
    let current_hash = match hash_skill_dir(&source_path) {
        Ok(h) => h,
        Err(e) => {
            warn!(
                skill = %skill.name,
                path = %source_path.display(),
                error = %e,
                "failed to re-hash skill dir for background assessment; dropping job"
            );
            let _ = store.delete_job(&skill.name, &expected_hash).await;
            return;
        }
    };
    if current_hash != expected_hash {
        info!(
            skill = %skill.name,
            "skill content changed before background assessment ran; dropping stale job"
        );
        let _ = store.delete_job(&skill.name, &expected_hash).await;
        return;
    }

    // Claim the job. Failure to mark InProgress is non-fatal: we'll
    // still try the LLM call; on the next startup the job status will
    // just look `Pending` which is semantically correct.
    if let Err(e) = store
        .set_job_status(
            &skill.name,
            &expected_hash,
            AssessmentJobStatus::InProgress,
            1,
            None,
        )
        .await
    {
        warn!(skill = %skill.name, error = %e, "could not mark risk job InProgress");
    }

    let mut attempts = 1u32;
    loop {
        match assess_full(llm, store, &skill, &source_path, &expected_hash).await {
            Ok(level) => {
                info!(
                    skill = %skill.name,
                    level = %level.as_str(),
                    "background full-scope risk assessment completed"
                );
                return;
            }
            Err(err) if attempts >= MAX_ATTEMPTS => {
                warn!(
                    skill = %skill.name,
                    attempts,
                    error = %err,
                    "giving up on background risk assessment"
                );
                let _ = store
                    .set_job_status(
                        &skill.name,
                        &expected_hash,
                        AssessmentJobStatus::Failed,
                        attempts,
                        Some(err.as_str()),
                    )
                    .await;
                return;
            }
            Err(err) => {
                attempts += 1;
                warn!(
                    skill = %skill.name,
                    attempts,
                    error = %err,
                    "background risk assessment failed; will retry"
                );
                let _ = store
                    .set_job_status(
                        &skill.name,
                        &expected_hash,
                        AssessmentJobStatus::Pending,
                        attempts,
                        Some(err.as_str()),
                    )
                    .await;
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

/// One end-to-end full-scope assessment attempt. Returns the stored
/// level on success; the verdict and job row are persisted before
/// returning.
async fn assess_full(
    llm: &BoundBilledLlm,
    store: &dyn SkillRiskStore,
    skill: &SkillDefinition,
    source_path: &std::path::Path,
    hash: &str,
) -> Result<RiskLevel, String> {
    let files = crate::assessor::read_skill_files(source_path)
        .map_err(|e| format!("read skill files: {e}"))?;
    let messages = build_messages(skill, Scope::Full, &files);
    let request = ChatRequest {
        messages,
        temperature: Some(0.0),
        tools: vec![],
    };
    let response = llm
        .chat(&request)
        .await
        .map_err(|e| format!("llm call: {e}"))?
        .response;
    let (level, rationale) = parse_verdict(&response.content)
        .ok_or_else(|| format!("unparsable LLM reply: {}", preview(&response.content)))?;

    let verdict = RiskVerdict {
        skill_name: skill.name.clone(),
        content_hash: hash.to_string(),
        level,
        rationale,
        model: llm.model_info().id.clone(),
        assessed_at: chrono::Utc::now().timestamp_micros(),
    };
    store
        .put(&verdict)
        .await
        .map_err(|e| format!("persist verdict: {e}"))?;
    store
        .delete_job(&skill.name, hash)
        .await
        .map_err(|e| format!("delete job row: {e}"))?;
    Ok(level)
}

fn preview(s: &str) -> String {
    const MAX: usize = 160;
    if s.len() <= MAX {
        s.to_string()
    } else {
        let mut end = MAX;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Convert a persisted job row back into an in-memory `BackgroundJob`
/// at startup. Jobs whose `source_path` no longer exists on disk are
/// dropped — the worker would fail-and-retry-forever otherwise, and
/// nothing downstream depends on a verdict for a skill that isn't
/// loaded.
pub(crate) async fn materialise_for_recovery(
    store: &dyn SkillRiskStore,
    persisted: AssessmentJob,
    skill: Option<SkillDefinition>,
) -> Option<BackgroundJob> {
    let Some(skill) = skill else {
        debug!(
            skill = %persisted.skill_name,
            "dropping recovered risk job: skill no longer registered"
        );
        let _ = store
            .delete_job(&persisted.skill_name, &persisted.content_hash)
            .await;
        return None;
    };
    let source_path = PathBuf::from(&persisted.source_path);
    if !source_path.is_dir() {
        debug!(
            skill = %persisted.skill_name,
            path = %source_path.display(),
            "dropping recovered risk job: source path missing"
        );
        let _ = store
            .delete_job(&persisted.skill_name, &persisted.content_hash)
            .await;
        return None;
    }
    Some(BackgroundJob {
        skill,
        source_path,
        expected_hash: persisted.content_hash,
    })
}
