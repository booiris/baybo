//! [`SubagentRegistry`] — process-wide lookup of typed subagent profiles.
//!
//! Pattern mirrors `baybo-skills::SkillRegistry`: interior mutability,
//! `Arc<SubagentRegistry>` shared across the agent loop, the
//! `spawn_subagent` builtin (for dynamic description rendering), and
//! the CLI (for `baybo agents list`). `register` overwrites by name,
//! `load_dir` remembers the path so `reload` can replay disk state, and
//! `register_builtins` populates the catch-all `general-purpose` entry
//! so a fresh workspace boots with a usable spawn target.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::RwLock;
use tracing::{debug, warn};

use crate::loader::load_profile_from_file;
use crate::profile::{SubagentProfile, SubagentProfileSummary};

/// Central registry for typed subagent profiles.
pub struct SubagentRegistry {
    profiles: DashMap<String, SubagentProfile>,
    /// Directories passed to `load_dir`, first-seen order. `reload`
    /// replays these scans so live edits land on the next refresh.
    load_dirs: RwLock<Vec<PathBuf>>,
    /// Bumped on every mutation (`register` / `remove` / `reload` /
    /// `register_builtins`). Consumers that cache derived data —
    /// notably the `spawn_subagent` tool's dynamic catalogue, which
    /// the LLM sees every turn — read this once, compare against
    /// their cached snapshot, and rebuild only on mismatch.
    version: AtomicU64,
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self {
            profiles: DashMap::new(),
            load_dirs: RwLock::new(Vec::new()),
            version: AtomicU64::new(0),
        }
    }

    /// Monotonically-increasing snapshot id. Bumped on every catalogue
    /// mutation. Cache invalidation for derived views — read once,
    /// compare on the next read, rebuild only on mismatch.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Register a profile. Overwrites any existing entry by name.
    pub fn register(&self, profile: SubagentProfile) {
        debug!(name = %profile.name, "registering subagent profile");
        self.profiles.insert(profile.name.clone(), profile);
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Register every built-in profile (`crate::builtin::all`). Loaded
    /// from markdown bundled in the binary so a fresh workspace has at
    /// least the catch-all `general-purpose` target available.
    /// Workspace profiles registered later with the same name override.
    pub fn register_builtins(&self) -> usize {
        let mut loaded = 0;
        for profile in crate::builtin::all() {
            self.register(profile);
            loaded += 1;
        }
        loaded
    }

    /// Load every `<dir>/<name>.md` (single-file profiles) under `dir`.
    /// Existing profiles with the same name are overwritten.
    ///
    /// Missing or unreadable `dir` is treated as empty (debug log only).
    /// Individual files that fail to parse are logged and skipped — one
    /// broken profile must not block the rest. Returns the number of
    /// profiles successfully loaded this call.
    pub fn load_dir(&self, dir: &Path) -> usize {
        {
            let mut dirs = self.load_dirs.write();
            if !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_path_buf());
            }
        }
        self.scan_dir(dir)
    }

    fn scan_dir(&self, dir: &Path) -> usize {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(err) => {
                debug!(
                    path = %dir.display(),
                    error = %err,
                    "subagent profile directory not available; skipping"
                );
                return 0;
            }
        };

        let mut loaded = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext_is_md = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if !ext_is_md {
                continue;
            }
            match load_profile_from_file(&path) {
                Ok(profile) => {
                    self.register(profile);
                    loaded += 1;
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to load subagent profile");
                }
            }
        }
        loaded
    }

    /// Re-scan every directory previously passed to `load_dir`. Profiles
    /// removed from disk drop out, edits land, new files appear.
    /// Returns the total profile count after reload.
    ///
    /// Authoritative-disk: programmatic `register` calls without a
    /// matching disk file are cleared. Built-ins are NOT re-registered
    /// here — the caller drives `register_builtins` ↔ `reload` ordering
    /// explicitly.
    pub fn reload(&self) -> usize {
        let dirs: Vec<PathBuf> = self.load_dirs.read().clone();
        self.profiles.clear();
        self.version.fetch_add(1, Ordering::Release);
        for dir in &dirs {
            self.scan_dir(dir);
        }
        self.profiles.len()
    }

    /// Look up a profile by name, returning a cloned definition.
    pub fn get(&self, name: &str) -> Option<SubagentProfile> {
        self.profiles.get(name).map(|e| e.value().clone())
    }

    /// True iff no profiles are registered.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Remove a profile by name. Mostly for tests; production drops via
    /// `reload`.
    pub fn remove(&self, name: &str) -> Option<SubagentProfile> {
        let dropped = self.profiles.remove(name).map(|(_, v)| v);
        if dropped.is_some() {
            self.version.fetch_add(1, Ordering::Release);
        }
        dropped
    }

    /// All registered profile names. Order is registration-arbitrary —
    /// callers needing stable order use `all_summaries_sorted`.
    pub fn list(&self) -> Vec<String> {
        self.profiles.iter().map(|e| e.key().clone()).collect()
    }

    /// Every registered profile, sorted by name. Heavy — clones the
    /// full `system_prompt` body. Use `all_summaries_sorted` for the
    /// hot path that just lists names + descriptions.
    pub fn all_sorted(&self) -> Vec<SubagentProfile> {
        let mut out: Vec<SubagentProfile> =
            self.profiles.iter().map(|e| e.value().clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Case-insensitive substring search over name + description.
    /// Empty query returns every profile (sorted by name).
    pub fn search(&self, query: &str) -> Vec<SubagentProfileSummary> {
        let needle = query.trim().to_ascii_lowercase();
        let mut hits: Vec<SubagentProfileSummary> = self
            .profiles
            .iter()
            .filter(|e| {
                if needle.is_empty() {
                    return true;
                }
                let p = e.value();
                p.name.to_ascii_lowercase().contains(&needle)
                    || p.description.to_ascii_lowercase().contains(&needle)
            })
            .map(|e| SubagentProfileSummary::from(e.value()))
            .collect();
        hits.sort_by(|a, b| a.name.cmp(&b.name));
        hits
    }

    /// Lightweight equivalent of `all_sorted` that skips `system_prompt`
    /// cloning. The `spawn_subagent` description renderer calls this
    /// every LLM turn; allocating one full prompt body per profile per
    /// turn would dominate the per-call overhead.
    pub fn all_summaries_sorted(&self) -> Vec<SubagentProfileSummary> {
        let mut out: Vec<SubagentProfileSummary> = self
            .profiles
            .iter()
            .map(|e| SubagentProfileSummary::from(e.value()))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ArtifactSource, ModelTier, TrustLevel};

    fn mk(name: &str) -> SubagentProfile {
        SubagentProfile {
            name: name.into(),
            version: "0.1.0".into(),
            description: format!("{name} description"),
            system_prompt: format!("You are the {name} subagent."),
            default_tier: Some(ModelTier::Balanced),
            source: ArtifactSource::Workspace,
            trust_level: TrustLevel::Trusted,
            source_path: None,
            tools: None,
        }
    }

    #[test]
    fn register_and_get_roundtrip() {
        let reg = SubagentRegistry::new();
        reg.register(mk("explorer"));
        let got = reg.get("explorer").unwrap();
        assert_eq!(got.name, "explorer");
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn all_summaries_sorted_alphabetises() {
        let reg = SubagentRegistry::new();
        reg.register(mk("zeta"));
        reg.register(mk("alpha"));
        reg.register(mk("mid"));
        let names: Vec<String> = reg
            .all_summaries_sorted()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn load_dir_reads_single_file_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.md");
        std::fs::write(
            &path,
            "---\nname: scout\ndescription: scouts ahead\ndefault_tier: fast\n---\nYou are scout.\n",
        )
        .unwrap();

        let dud = dir.path().join("not-a-profile.txt");
        std::fs::write(&dud, "ignored").unwrap();

        let reg = SubagentRegistry::new();
        let loaded = reg.load_dir(dir.path());
        assert_eq!(loaded, 1);
        let scout = reg.get("scout").unwrap();
        assert_eq!(scout.default_tier, Some(ModelTier::Lite));
        assert!(scout.source_path.is_some());
    }

    #[test]
    fn load_dir_skips_broken_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("good.md"),
            "---\nname: good\ndescription: works\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("broken.md"),
            "---\nname: bad name with spaces\ndescription: d\n---\nbody\n",
        )
        .unwrap();

        let reg = SubagentRegistry::new();
        let loaded = reg.load_dir(dir.path());
        assert_eq!(loaded, 1);
        assert!(reg.get("good").is_some());
    }

    #[test]
    fn load_dir_missing_directory_returns_zero() {
        let reg = SubagentRegistry::new();
        let loaded = reg.load_dir(Path::new("/definitely/does/not/exist/baybo-agents"));
        assert_eq!(loaded, 0);
    }

    #[test]
    fn reload_picks_up_disk_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.md");
        std::fs::write(&path, "---\nname: p\ndescription: v1\n---\nbody\n").unwrap();

        let reg = SubagentRegistry::new();
        assert_eq!(reg.load_dir(dir.path()), 1);
        assert_eq!(reg.get("p").unwrap().description, "v1");

        std::fs::write(&path, "---\nname: p\ndescription: v2\n---\nbody2\n").unwrap();
        assert_eq!(reg.reload(), 1);
        assert_eq!(reg.get("p").unwrap().description, "v2");

        std::fs::remove_file(&path).unwrap();
        assert_eq!(reg.reload(), 0);
        assert!(reg.get("p").is_none());
    }

    #[test]
    fn reload_without_prior_load_dir_clears_to_empty() {
        let reg = SubagentRegistry::new();
        reg.register(mk("ephemeral"));
        assert_eq!(reg.reload(), 0);
        assert!(reg.get("ephemeral").is_none());
    }

    #[test]
    fn register_overwrites_by_name() {
        let reg = SubagentRegistry::new();
        let mut a = mk("foo");
        a.description = "first".into();
        reg.register(a);
        let mut b = mk("foo");
        b.description = "second".into();
        reg.register(b);
        assert_eq!(reg.get("foo").unwrap().description, "second");
    }
}
