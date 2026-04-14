//! `SkillAssessor` — orchestrates hash → cache lookup → LLM call.

use std::path::Path;
use std::sync::Arc;

use aura_llm::{ChatRequest, LlmClient};
use aura_skills::SkillDefinition;
use aura_storage::{AssessmentJob, AssessmentJobStatus, RiskVerdict, SkillRiskStore};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::hash::{hash_skill_dir, hash_skill_primary, should_tier};
use crate::prompt::{Scope, build_messages, parse_verdict};
use crate::queue::{BackgroundJob, materialise_for_recovery, spawn_worker};

#[derive(Debug, Error)]
pub enum AssessError {
    #[error("skill has no on-disk source_path; cannot assess")]
    NoSourcePath,
    #[error("hashing skill dir failed: {0}")]
    Hash(#[from] std::io::Error),
    #[error("risk store: {0}")]
    Store(#[from] aura_storage::StorageError),
    #[error("LLM call failed: {0}")]
    Llm(String),
    #[error("LLM reply did not parse as a verdict: {preview}")]
    UnparsableReply { preview: String },
}

/// Which scope a returned verdict covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssessmentScope {
    /// SKILL.md only. A full-scope check may still be pending in the
    /// background; consult `AssessedSkill::background_pending`.
    Primary,
    /// Full directory tree (SKILL.md + all helpers).
    Full,
}

impl AssessmentScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AssessmentScope::Primary => "primary",
            AssessmentScope::Full => "full",
        }
    }
}

/// Richer result from `SkillAssessor::check`: the verdict plus
/// meta-information the caller needs to present it honestly.
#[derive(Debug, Clone)]
pub struct AssessedSkill {
    pub verdict: RiskVerdict,
    pub scope: AssessmentScope,
    /// True when a full-scope assessment has been enqueued and is not
    /// yet complete. The UI can surface "primary verdict — full check
    /// in progress" so the operator knows the answer may sharpen.
    pub background_pending: bool,
}

/// LLM-backed risk assessor with a persistent verdict cache.
///
/// `check` is idempotent and safe to call every time a skill is about
/// to be used. Small skills produce a single full-scope verdict on the
/// first call; larger skills switch to a tiered flow where a
/// primary-scope verdict on `SKILL.md` is returned synchronously and
/// the full-scope check is delegated to a background worker.
pub struct SkillAssessor {
    llm: Arc<LlmClient>,
    store: Arc<dyn SkillRiskStore>,
    /// None when the assessor is configured without a worker (e.g.
    /// short-lived argv commands). `check` falls back to fully
    /// synchronous assessment — tiered mode requires the worker.
    background: Option<mpsc::Sender<BackgroundJob>>,
}

impl SkillAssessor {
    /// Construct an assessor with a background worker for tiered
    /// assessment. The worker is spawned on the current Tokio runtime
    /// and lives as long as any sender is held.
    pub fn with_background_worker(llm: Arc<LlmClient>, store: Arc<dyn SkillRiskStore>) -> Self {
        let tx = spawn_worker(Arc::clone(&llm), Arc::clone(&store), 64);
        Self {
            llm,
            store,
            background: Some(tx),
        }
    }

    /// Load persisted pending jobs and re-enqueue them for the worker.
    ///
    /// Called once at startup, after the assessor is constructed and
    /// the skill registry is populated. `lookup` maps a skill name to
    /// its current definition (if the skill is still registered). Jobs
    /// for unknown or missing-on-disk skills are deleted from the
    /// store, not re-enqueued.
    pub async fn recover_pending_jobs(
        &self,
        lookup: impl Fn(&str) -> Option<SkillDefinition>,
    ) -> Result<usize, AssessError> {
        let Some(tx) = self.background.as_ref() else {
            return Ok(0);
        };
        let rows = self.store.load_pending_jobs().await?;
        let mut requeued = 0usize;
        for row in rows {
            let def = lookup(&row.skill_name);
            let Some(job) = materialise_for_recovery(self.store.as_ref(), row, def).await else {
                continue;
            };
            if tx.send(job).await.is_err() {
                warn!("background worker channel closed during recovery; stopping");
                break;
            }
            requeued += 1;
        }
        Ok(requeued)
    }

    /// Return the best available verdict for `skill`.
    ///
    /// Small skills: single synchronous LLM call, full-scope verdict.
    /// Large skills: primary-scope verdict returned synchronously, full
    /// scope enqueued for background assessment.
    pub async fn check(&self, skill: &SkillDefinition) -> Result<AssessedSkill, AssessError> {
        let dir = skill
            .source_path
            .as_deref()
            .ok_or(AssessError::NoSourcePath)?;

        let full_hash = hash_skill_dir(dir)?;

        // Authoritative cache hit — nothing else to do.
        if let Some(cached) = self.store.get(&skill.name, &full_hash).await? {
            debug!(
                skill = %skill.name,
                hash = %full_hash,
                level = %cached.level.as_str(),
                "full-scope risk cache hit"
            );
            return Ok(AssessedSkill {
                verdict: cached,
                scope: AssessmentScope::Full,
                background_pending: false,
            });
        }

        let tier = should_tier(dir)?;

        // Below the threshold: keep the simple single-call path so
        // small skills don't pay for worker coordination.
        if !tier || self.background.is_none() {
            let verdict = self.call_llm_full(skill, dir, full_hash).await?;
            self.store.put(&verdict).await?;
            return Ok(AssessedSkill {
                verdict,
                scope: AssessmentScope::Full,
                background_pending: false,
            });
        }

        // Tiered flow: primary cache → primary LLM, then enqueue full.
        let primary_verdict = match hash_skill_primary(dir)? {
            Some(primary_hash) => {
                if let Some(cached) = self.store.get(&skill.name, &primary_hash).await? {
                    debug!(
                        skill = %skill.name,
                        hash = %primary_hash,
                        level = %cached.level.as_str(),
                        "primary-scope risk cache hit"
                    );
                    cached
                } else {
                    let v = self.call_llm_primary(skill, dir, primary_hash).await?;
                    self.store.put(&v).await?;
                    v
                }
            }
            None => {
                // No SKILL.md to isolate — fall back to full-scope
                // synchronous assessment. Better to pay the latency
                // once than to serve the caller with no verdict.
                let verdict = self.call_llm_full(skill, dir, full_hash).await?;
                self.store.put(&verdict).await?;
                return Ok(AssessedSkill {
                    verdict,
                    scope: AssessmentScope::Full,
                    background_pending: false,
                });
            }
        };

        self.enqueue_full_job(skill, dir, &full_hash).await;

        Ok(AssessedSkill {
            verdict: primary_verdict,
            scope: AssessmentScope::Primary,
            background_pending: true,
        })
    }

    /// Look up a cached verdict without calling the LLM. Preference
    /// order: full-scope > primary-scope. Returned tuple includes the
    /// scope so the caller can tell whether a full check is still
    /// owed. Used by hot-path gates that can't afford a blocking call.
    pub async fn cached(
        &self,
        skill: &SkillDefinition,
    ) -> Result<Option<(RiskVerdict, AssessmentScope)>, AssessError> {
        let Some(dir) = skill.source_path.as_deref() else {
            return Ok(None);
        };
        let full_hash = hash_skill_dir(dir)?;
        if let Some(v) = self.store.get(&skill.name, &full_hash).await? {
            return Ok(Some((v, AssessmentScope::Full)));
        }
        if let Some(primary_hash) = hash_skill_primary(dir)?
            && let Some(v) = self.store.get(&skill.name, &primary_hash).await?
        {
            return Ok(Some((v, AssessmentScope::Primary)));
        }
        Ok(None)
    }

    async fn call_llm_primary(
        &self,
        skill: &SkillDefinition,
        dir: &Path,
        hash: String,
    ) -> Result<RiskVerdict, AssessError> {
        let primary = dir.join(crate::hash::PRIMARY_FILE);
        let bytes = std::fs::read(&primary)?;
        let files = vec![(crate::hash::PRIMARY_FILE.to_string(), bytes)];
        self.call_llm(skill, Scope::Primary, &files, hash).await
    }

    async fn call_llm_full(
        &self,
        skill: &SkillDefinition,
        dir: &Path,
        hash: String,
    ) -> Result<RiskVerdict, AssessError> {
        let files = read_skill_files(dir)?;
        self.call_llm(skill, Scope::Full, &files, hash).await
    }

    async fn call_llm(
        &self,
        skill: &SkillDefinition,
        scope: Scope,
        files: &[(String, Vec<u8>)],
        hash: String,
    ) -> Result<RiskVerdict, AssessError> {
        let messages = build_messages(skill, scope, files);
        let request = ChatRequest {
            messages,
            temperature: Some(0.0),
            tools: vec![],
        };
        let response = self
            .llm
            .chat(&request)
            .await
            .map_err(|e| AssessError::Llm(e.to_string()))?;

        let (level, rationale) = match parse_verdict(&response.content) {
            Some(v) => v,
            None => {
                warn!(
                    skill = %skill.name,
                    scope = %scope.as_str(),
                    reply = %response.content,
                    "assessor reply did not parse"
                );
                let preview = truncate_preview(&response.content);
                return Err(AssessError::UnparsableReply { preview });
            }
        };

        Ok(RiskVerdict {
            skill_name: skill.name.clone(),
            content_hash: hash,
            level,
            rationale,
            model: self.llm.model_info().id.clone(),
            assessed_at: chrono::Utc::now().timestamp(),
        })
    }

    /// Persist a job row and hand the job to the background worker.
    /// Errors are logged, not propagated: failing to enqueue does not
    /// change the caller-visible primary verdict, and the job row (if
    /// it was written) will be retried on the next startup.
    async fn enqueue_full_job(&self, skill: &SkillDefinition, dir: &Path, full_hash: &str) {
        let Some(tx) = self.background.as_ref() else {
            return;
        };
        let now = chrono::Utc::now().timestamp();
        let job_row = AssessmentJob {
            skill_name: skill.name.clone(),
            content_hash: full_hash.to_string(),
            source_path: dir.display().to_string(),
            status: AssessmentJobStatus::Pending,
            attempts: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = self.store.upsert_job(&job_row).await {
            warn!(
                skill = %skill.name,
                error = %e,
                "failed to persist background risk-assessment job; skipping enqueue"
            );
            return;
        }
        let in_memory = BackgroundJob {
            skill: skill.clone(),
            source_path: dir.to_path_buf(),
            expected_hash: full_hash.to_string(),
        };
        if tx.try_send(in_memory).is_err() {
            debug!(
                skill = %skill.name,
                "background queue full or closed; job row will be picked up on next startup"
            );
        }
    }
}

fn truncate_preview(s: &str) -> String {
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

/// Load every file under `dir` as `(relative-path, bytes)`. Sorted for
/// deterministic prompt ordering.
pub(crate) fn read_skill_files(dir: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            // Symlinks recorded by the hash but skipped when
            // collecting content — we don't want to follow out of the
            // skill dir, and an in-tree symlink would just duplicate
            // its target in the prompt.
            continue;
        }
        if meta.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if meta.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_str = rel
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            let bytes = std::fs::read(&path)?;
            out.push((rel_str, bytes));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn read_skill_files_reads_nested_tree() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "body").unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts/helper.sh"), "#!/bin/sh\n").unwrap();

        let files = read_skill_files(dir.path()).unwrap();
        let names: Vec<_> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["SKILL.md", "scripts/helper.sh"]);
    }

    #[test]
    fn truncate_preview_respects_char_boundary() {
        // Multibyte char landing on a truncation boundary must not panic.
        let s = "A".repeat(158) + "中文";
        let out = truncate_preview(&s);
        assert!(out.ends_with('…'));
    }
}
