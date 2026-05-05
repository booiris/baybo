//! Categorized inventory of helper files alongside `SKILL.md`. Walked
//! at registry-load time and cached on [`crate::SkillDefinition`] so
//! the `Skill` tool's Mode 1 hot path doesn't re-stat the directory
//! on every invocation.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::{MAX_SKILL_DIR_BYTES, MAX_SKILL_DIR_FILES};

const REFERENCES: &str = "references";
const TEMPLATES: &str = "templates";
const SCRIPTS: &str = "scripts";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkedFiles {
    pub references: Vec<String>,
    pub templates: Vec<String>,
    pub scripts: Vec<String>,
    pub other: Vec<String>,
}

impl LinkedFiles {
    pub fn to_json(&self) -> Value {
        json!({
            REFERENCES: self.references,
            TEMPLATES: self.templates,
            SCRIPTS: self.scripts,
            "other": self.other,
        })
    }
}

/// Walk `skill_dir` and bucket every file (other than `SKILL.md`
/// itself) by its top-level subdirectory. Symlinks are not followed.
/// Bounded by [`MAX_SKILL_DIR_FILES`] / [`MAX_SKILL_DIR_BYTES`] so a
/// pathological tree is rejected uniformly with the assessor's
/// hashing path.
pub fn enumerate(skill_dir: &Path) -> Result<LinkedFiles, String> {
    let mut out = LinkedFiles::default();
    let mut total_bytes: u64 = 0;
    let mut total_files: usize = 0;

    for entry in WalkDir::new(skill_dir).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|err| format!("walk skill dir failed: {err}"))?;

        if !entry.file_type().is_file() {
            continue;
        }
        if entry.depth() == 1 && entry.file_name() == "SKILL.md" {
            continue;
        }

        total_files += 1;
        if total_files > MAX_SKILL_DIR_FILES {
            return Err(format!(
                "skill directory contains more than {MAX_SKILL_DIR_FILES} files"
            ));
        }
        if let Ok(meta) = entry.metadata() {
            total_bytes = total_bytes.saturating_add(meta.len());
            if total_bytes > MAX_SKILL_DIR_BYTES {
                return Err(format!(
                    "skill directory exceeds {MAX_SKILL_DIR_BYTES} bytes"
                ));
            }
        }

        let rel = match entry.path().strip_prefix(skill_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().into_owned();
        match top_level_dir(rel) {
            Some(REFERENCES) => out.references.push(rel_str),
            Some(TEMPLATES) => out.templates.push(rel_str),
            Some(SCRIPTS) => out.scripts.push(rel_str),
            _ => out.other.push(rel_str),
        }
    }

    out.references.sort();
    out.templates.sort();
    out.scripts.sort();
    out.other.sort();
    Ok(out)
}

fn top_level_dir(rel: &Path) -> Option<&str> {
    match rel.components().next()? {
        std::path::Component::Normal(name) => name.to_str(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn buckets_top_level_subdirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "x").unwrap();
        fs::create_dir(root.join("references")).unwrap();
        fs::write(root.join("references/a.md"), "x").unwrap();
        fs::create_dir(root.join("templates")).unwrap();
        fs::write(root.join("templates/c.yaml"), "x").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/s.sh"), "x").unwrap();
        fs::write(root.join("misc.txt"), "x").unwrap();

        let lf = enumerate(root).unwrap();
        assert_eq!(lf.references, vec!["references/a.md"]);
        assert_eq!(lf.templates, vec!["templates/c.yaml"]);
        assert_eq!(lf.scripts, vec!["scripts/s.sh"]);
        assert_eq!(lf.other, vec!["misc.txt"]);
    }

    #[test]
    fn skill_md_is_skipped() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "x").unwrap();
        let lf = enumerate(dir.path()).unwrap();
        assert!(
            lf.references.is_empty()
                && lf.templates.is_empty()
                && lf.scripts.is_empty()
                && lf.other.is_empty()
        );
    }
}
