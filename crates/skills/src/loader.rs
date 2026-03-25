use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aura_core::AuraError;
use tracing::{debug, info, warn};

use crate::SkillDefinition;
use crate::registry::SkillRegistry;

/// Loads a single skill definition from a JSON file.
pub async fn load_from_file(path: &Path) -> aura_core::Result<SkillDefinition> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| {
        AuraError::Config(format!("failed to read skill file {}: {e}", path.display()))
    })?;

    let skill: SkillDefinition = serde_json::from_str(&content).map_err(|e| {
        AuraError::Serialization(format!(
            "failed to parse skill file {}: {e}",
            path.display()
        ))
    })?;

    Ok(skill)
}

/// Loads all `.json` skill files from a directory.
/// Returns successfully loaded skills and any errors encountered.
/// An error reading the directory itself is propagated; individual file
/// failures are collected but do not stop loading of other files.
pub async fn load_from_dir(
    dir: &Path,
) -> aura_core::Result<(Vec<SkillDefinition>, Vec<AuraError>)> {
    let mut entries = tokio::fs::read_dir(dir).await.map_err(|e| {
        AuraError::Config(format!(
            "failed to read skill directory {}: {e}",
            dir.display()
        ))
    })?;

    let mut skills = Vec::new();
    let mut errors = Vec::new();

    loop {
        let entry = entries.next_entry().await.map_err(|e| {
            AuraError::Config(format!(
                "failed to iterate skill directory {}: {e}",
                dir.display()
            ))
        })?;

        let entry = match entry {
            Some(e) => e,
            None => break,
        };

        let path = entry.path();
        let is_json = path.extension().is_some_and(|ext| ext == "json");

        if !is_json {
            continue;
        }

        match load_from_file(&path).await {
            Ok(skill) => skills.push(skill),
            Err(e) => errors.push(e),
        }
    }

    Ok((skills, errors))
}

/// Poll-based hot-reload watcher for skill files.
///
/// Periodically checks a directory for new, modified, or removed skill files
/// and updates the registry accordingly. Uses file modification timestamps
/// to detect changes without requiring a platform-specific file watcher.
pub struct SkillHotReloader {
    dir: PathBuf,
    registry: Arc<SkillRegistry>,
    poll_interval: Duration,
    /// Tracks last-seen modification times for loaded files.
    file_states: HashMap<PathBuf, SystemTime>,
}

impl SkillHotReloader {
    pub fn new(dir: PathBuf, registry: Arc<SkillRegistry>, poll_interval: Duration) -> Self {
        Self {
            dir,
            registry,
            poll_interval,
            file_states: HashMap::new(),
        }
    }

    /// Perform a single reload pass: detect changes and update the registry.
    /// Returns the number of skills reloaded.
    pub async fn reload_once(&mut self) -> aura_core::Result<usize> {
        let mut current_files = HashMap::new();
        let mut reloaded = 0usize;

        let mut entries = tokio::fs::read_dir(&self.dir).await.map_err(|e| {
            AuraError::Config(format!(
                "failed to read skill directory {}: {e}",
                self.dir.display()
            ))
        })?;

        loop {
            let entry = entries.next_entry().await.map_err(|e| {
                AuraError::Config(format!(
                    "failed to iterate skill directory {}: {e}",
                    self.dir.display()
                ))
            })?;

            let entry = match entry {
                Some(e) => e,
                None => break,
            };

            let path = entry.path();
            let is_json = path.extension().is_some_and(|ext| ext == "json");
            if !is_json {
                continue;
            }

            let modified = match tokio::fs::metadata(&path).await {
                Ok(meta) => meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to stat skill file");
                    continue;
                }
            };

            current_files.insert(path.clone(), modified);

            let needs_reload = match self.file_states.get(&path) {
                Some(prev_modified) => *prev_modified != modified,
                None => true,
            };

            if needs_reload {
                match load_from_file(&path).await {
                    Ok(skill) => {
                        info!(
                            skill_name = %skill.name,
                            version = %skill.version,
                            path = %path.display(),
                            "hot-reloaded skill"
                        );
                        self.registry.register(skill).await;
                        reloaded += 1;
                    }
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to reload skill file"
                        );
                    }
                }
            }
        }

        // Detect removed files — skills whose files no longer exist.
        let removed_paths: Vec<PathBuf> = self
            .file_states
            .keys()
            .filter(|p| !current_files.contains_key(*p))
            .cloned()
            .collect();

        for path in &removed_paths {
            debug!(path = %path.display(), "skill file removed");
        }

        self.file_states = current_files;

        Ok(reloaded)
    }

    /// Spawn a background task that periodically reloads skills.
    /// Returns a handle that can be used to abort the watcher.
    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.poll_interval);
            loop {
                ticker.tick().await;
                match self.reload_once().await {
                    Ok(0) => {}
                    Ok(n) => debug!(count = n, "hot-reloaded skills"),
                    Err(e) => warn!(error = %e, "skill hot-reload pass failed"),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_load_from_file() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("skills_loader_test");
        let _ = tokio::fs::create_dir_all(&dir).await;

        let skill_json = r#"{
            "name": "test-skill",
            "version": "1.0.0",
            "description": "A test skill",
            "trigger": "AgentDecision",
            "prompt_template": "Do the thing",
            "allowed_tools": [],
            "post_processing": null,
            "source": "Workspace",
            "trust_level": "Trusted",
            "requirements": {},
            "token_budget_hint": 100
        }"#;

        let file_path = dir.join("test-skill.json");
        tokio::fs::write(&file_path, skill_json).await.unwrap();

        let skill = load_from_file(&file_path).await.unwrap();
        assert_eq!(skill.name, "test-skill");
        assert_eq!(skill.version, "1.0.0");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_load_from_dir_partial_failure() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("skills_loader_dir_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let _ = tokio::fs::create_dir_all(&dir).await;

        // Valid skill
        let valid = r#"{
            "name": "good-skill",
            "version": "1.0.0",
            "description": "Valid",
            "trigger": "AgentDecision",
            "prompt_template": "Do it",
            "allowed_tools": [],
            "source": "Workspace",
            "trust_level": "Trusted",
            "requirements": {},
            "token_budget_hint": 50
        }"#;
        tokio::fs::write(dir.join("good.json"), valid)
            .await
            .unwrap();

        // Invalid skill
        tokio::fs::write(dir.join("bad.json"), "not json")
            .await
            .unwrap();

        // Non-json file (should be skipped)
        tokio::fs::write(dir.join("readme.txt"), "ignore me")
            .await
            .unwrap();

        let (skills, errors) = load_from_dir(&dir).await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good-skill");
        assert_eq!(errors.len(), 1);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_load_from_missing_dir() {
        let result = load_from_dir(Path::new("/nonexistent/skills/dir")).await;
        assert!(result.is_err());
    }
}
