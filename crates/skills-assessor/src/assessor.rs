//! `SkillAssessor` — orchestrates hash → cache lookup → LLM call.

use std::path::Path;
use std::sync::Arc;

use aura_llm::{ChatRequest, LlmClient};
use aura_skills::SkillDefinition;
use aura_storage::{AssessmentJob, AssessmentJobStatus, RiskLevel, RiskVerdict, SkillRiskStore};
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
    /// Assessment was skipped (mode = `Off`); the accompanying verdict
    /// is a synthesized `Safe` and carries no rationale.
    Disabled,
    /// SKILL.md only. A full-scope check may still be pending in the
    /// background; consult `AssessedSkill::background_pending`.
    Primary,
    /// Full directory tree (SKILL.md + all helpers).
    Full,
}

impl AssessmentScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AssessmentScope::Disabled => "disabled",
            AssessmentScope::Primary => "primary",
            AssessmentScope::Full => "full",
        }
    }
}

/// How `SkillAssessor::check` judges a skill. Mirrors
/// `aura_config::RiskCheckConfig`; bootstrap maps between them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AssessmentMode {
    /// Skip the LLM classifier. Every skill comes back `Safe`.
    Off,
    /// Read and classify `SKILL.md` only. Default.
    #[default]
    Primary,
    /// Read and classify the whole directory tree synchronously.
    Full,
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
/// to be used. The `mode` passed at construction time decides whether
/// the classifier runs at all, and if so which scope it judges —
/// `Primary` reads only `SKILL.md`, `Full` reads the whole tree.
pub struct SkillAssessor {
    llm: Arc<LlmClient>,
    store: Arc<dyn SkillRiskStore>,
    mode: AssessmentMode,
    /// Background worker handle. Under `Full` mode `check_full` tiers
    /// oversized skills here; recovery of persisted rows from previous
    /// runs also flows through it. `None` when the assessor was
    /// constructed without a runtime (argv one-shots).
    background: Option<mpsc::Sender<BackgroundJob>>,
}

impl SkillAssessor {
    /// Construct an assessor with a background worker attached for
    /// recovery of jobs persisted by previous runs. The worker is
    /// spawned on the current Tokio runtime and lives as long as any
    /// sender is held.
    pub fn with_background_worker(
        llm: Arc<LlmClient>,
        store: Arc<dyn SkillRiskStore>,
        mode: AssessmentMode,
    ) -> Self {
        let tx = spawn_worker(Arc::clone(&llm), Arc::clone(&store), 64);
        Self {
            llm,
            store,
            mode,
            background: Some(tx),
        }
    }

    pub fn mode(&self) -> AssessmentMode {
        self.mode
    }

    /// Load persisted pending jobs and re-enqueue them for the worker.
    ///
    /// Called once at startup, after the assessor is constructed and
    /// the skill registry is populated. `lookup` maps a skill name to
    /// its current definition (if the skill is still registered). Jobs
    /// for unknown or missing-on-disk skills are deleted from the
    /// store, not re-enqueued. Recovery runs regardless of the current
    /// `AssessmentMode` — rows that were committed to disk represent
    /// work already paid for, and flipping to `Off` suppresses new
    /// enqueues but should still let in-flight verdicts finish rather
    /// than stranding them until the operator flips back.
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
    /// `Off`     → synthesized `Safe`, no I/O.
    /// `Primary` → classify `SKILL.md`; helper files are ignored.
    /// `Full`    → classify the whole directory tree synchronously.
    pub async fn check(&self, skill: &SkillDefinition) -> Result<AssessedSkill, AssessError> {
        match self.mode {
            AssessmentMode::Off => Ok(disabled_verdict(skill)),
            AssessmentMode::Primary => self.check_primary(skill).await,
            AssessmentMode::Full => self.check_full(skill).await,
        }
    }

    async fn check_primary(&self, skill: &SkillDefinition) -> Result<AssessedSkill, AssessError> {
        let dir = skill
            .source_path
            .as_deref()
            .ok_or(AssessError::NoSourcePath)?;

        // No SKILL.md on disk → Primary mode has nothing to judge.
        // Return a synthesized Safe rather than silently escalating;
        // operators who want helper-script coverage must opt into Full.
        let Some(primary_hash) = hash_skill_primary(dir)? else {
            return Ok(disabled_verdict(skill));
        };

        if let Some(cached) = self.store.get(&skill.name, &primary_hash).await? {
            debug!(
                skill = %skill.name,
                hash = %primary_hash,
                level = %cached.level.as_str(),
                "primary-scope risk cache hit"
            );
            return Ok(AssessedSkill {
                verdict: cached,
                scope: AssessmentScope::Primary,
                background_pending: false,
            });
        }

        let verdict = self.call_llm_primary(skill, dir, primary_hash).await?;
        self.store.put(&verdict).await?;
        Ok(AssessedSkill {
            verdict,
            scope: AssessmentScope::Primary,
            background_pending: false,
        })
    }

    async fn check_full(&self, skill: &SkillDefinition) -> Result<AssessedSkill, AssessError> {
        let dir = skill
            .source_path
            .as_deref()
            .ok_or(AssessError::NoSourcePath)?;

        let full_hash = hash_skill_dir(dir)?;
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

        // Small skills classify synchronously; large ones degrade to
        // primary-sync + full-background so a chat turn doesn't block
        // on a big LLM prompt.
        if !should_tier(dir)? {
            let verdict = self.call_llm_full(skill, dir, full_hash).await?;
            self.store.put(&verdict).await?;
            return Ok(AssessedSkill {
                verdict,
                scope: AssessmentScope::Full,
                background_pending: false,
            });
        }

        let Some(primary_hash) = hash_skill_primary(dir)? else {
            // No SKILL.md to isolate — no way to tier cleanly; fall
            // through to sync full. For a registered skill this is a
            // misconfiguration but we'd rather classify than bail.
            let verdict = self.call_llm_full(skill, dir, full_hash).await?;
            self.store.put(&verdict).await?;
            return Ok(AssessedSkill {
                verdict,
                scope: AssessmentScope::Full,
                background_pending: false,
            });
        };

        // If we've tiered this skill before, the primary verdict is
        // already cached and the full job is already enqueued. Re-using
        // the cache entry avoids a duplicate channel send (which would
        // waste an LLM call on the worker side).
        let primary_verdict =
            if let Some(cached) = self.store.get(&skill.name, &primary_hash).await? {
                debug!(
                    skill = %skill.name,
                    hash = %primary_hash,
                    level = %cached.level.as_str(),
                    "primary-scope risk cache hit (tiered, full still pending)"
                );
                cached
            } else {
                let verdict = self.call_llm_primary(skill, dir, primary_hash).await?;
                self.store.put(&verdict).await?;
                self.enqueue_full(skill, dir, &full_hash).await?;
                verdict
            };

        Ok(AssessedSkill {
            verdict: primary_verdict,
            scope: AssessmentScope::Primary,
            background_pending: true,
        })
    }

    async fn enqueue_full(
        &self,
        skill: &SkillDefinition,
        dir: &Path,
        full_hash: &str,
    ) -> Result<(), AssessError> {
        let now = chrono::Utc::now().timestamp();
        let row = AssessmentJob {
            skill_name: skill.name.clone(),
            content_hash: full_hash.to_string(),
            source_path: dir.to_string_lossy().into_owned(),
            status: AssessmentJobStatus::Pending,
            attempts: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        self.store.upsert_job(&row).await?;

        if let Some(tx) = &self.background {
            let msg = BackgroundJob {
                skill: skill.clone(),
                source_path: dir.to_path_buf(),
                expected_hash: full_hash.to_string(),
            };
            if tx.send(msg).await.is_err() {
                warn!(
                    skill = %skill.name,
                    "background worker channel closed; full verdict will only arrive after restart"
                );
            }
        }
        Ok(())
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
}

/// Synthesize the verdict returned when the assessor is disabled (mode
/// `Off`) or the skill has no `SKILL.md` to isolate under `Primary`
/// mode. The hash is left empty on purpose — this verdict is never
/// persisted, so there's no cache key to bind.
fn disabled_verdict(skill: &SkillDefinition) -> AssessedSkill {
    AssessedSkill {
        verdict: RiskVerdict {
            skill_name: skill.name.clone(),
            content_hash: String::new(),
            level: RiskLevel::Safe,
            rationale: "skill risk assessment disabled".to_string(),
            model: String::new(),
            assessed_at: chrono::Utc::now().timestamp(),
        },
        scope: AssessmentScope::Disabled,
        background_pending: false,
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
