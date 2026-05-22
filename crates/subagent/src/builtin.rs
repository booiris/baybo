//! Built-in subagent profiles compiled into the binary.
//!
//! The pattern mirrors `aura-skills::builtin`: each profile is a single
//! `<name>.md` file under `builtin/`, embedded via `include_str!` at
//! build time, and surfaced through [`all`] for
//! [`SubagentRegistry::register_builtins`].
//!
//! Built-ins exist so a fresh workspace ships with at least the
//! catch-all `general-purpose` target — `spawn_subagent` must always
//! have something to resolve. Workspace profiles registered later with
//! the same name override built-ins.
//!
//! [`SubagentRegistry::register_builtins`]: crate::SubagentRegistry::register_builtins

use aura_model::{ArtifactSource, TrustLevel};

use crate::SubagentProfile;
use crate::loader::parse_profile_md;

const GENERAL_PURPOSE_MD: &str = include_str!("builtin/general_purpose.md");
const EXPLORER_MD: &str = include_str!("builtin/explorer.md");
const PLANNER_MD: &str = include_str!("builtin/planner.md");
const REVIEWER_MD: &str = include_str!("builtin/reviewer.md");

/// Registration table for built-in profiles. Each entry pairs the
/// default name (used when the frontmatter omits one) with the
/// embedded source. Adding a profile = one new file under
/// `builtin/` + one row here.
const BUILTIN_SOURCES: &[(&str, &str)] = &[
    ("general-purpose", GENERAL_PURPOSE_MD),
    ("explorer", EXPLORER_MD),
    ("planner", PLANNER_MD),
    ("reviewer", REVIEWER_MD),
];

/// All built-in profiles, in registration order. `register_builtins`
/// drives each through `register`, overwriting any prior entry by name.
/// A bundled profile that fails to parse logs an error and is skipped
/// — a botched built-in cannot crash boot.
pub fn all() -> Vec<SubagentProfile> {
    BUILTIN_SOURCES
        .iter()
        .filter_map(|(name, src)| parse_builtin(name, src))
        .collect()
}

/// Parse one bundled markdown source and stamp it as a built-in.
/// Parse failures here would mean a bundled file is corrupt — log and
/// skip rather than panic, so a botched built-in cannot crash boot.
fn parse_builtin(name: &str, content: &str) -> Option<SubagentProfile> {
    match parse_profile_md(content, name) {
        Ok(mut p) => {
            p.source = ArtifactSource::Inline;
            p.trust_level = TrustLevel::Trusted;
            p.source_path = None;
            Some(p)
        }
        Err(err) => {
            tracing::error!(
                builtin = %name,
                error = %err,
                "built-in subagent profile failed to parse; skipping"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::ModelTier;

    #[test]
    fn every_builtin_source_parses_cleanly() {
        for (name, src) in BUILTIN_SOURCES {
            let p = parse_profile_md(src, name).unwrap_or_else(|e| {
                panic!("built-in profile {name} failed to parse: {e}");
            });
            assert_eq!(&p.name, name, "frontmatter name must match registration");
            assert!(
                !p.description.trim().is_empty(),
                "built-in {name} must declare a non-empty description"
            );
            assert!(
                !p.system_prompt.trim().is_empty(),
                "built-in {name} must have a non-empty body"
            );
        }
    }

    #[test]
    fn all_returns_every_registered_builtin() {
        let built_in = all();
        let names: Vec<&str> = built_in.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"general-purpose"));
        assert!(names.contains(&"explorer"));
        assert!(names.contains(&"planner"));
        assert!(names.contains(&"reviewer"));
        for p in &built_in {
            assert_eq!(p.source, ArtifactSource::Inline);
            assert_eq!(p.trust_level, TrustLevel::Trusted);
            assert!(p.source_path.is_none());
        }
    }

    #[test]
    fn explorer_defaults_to_fast_tier() {
        let p = all().into_iter().find(|p| p.name == "explorer").unwrap();
        assert_eq!(p.default_tier, Some(ModelTier::Fast));
    }

    #[test]
    fn planner_defaults_to_deep_tier() {
        let p = all().into_iter().find(|p| p.name == "planner").unwrap();
        assert_eq!(p.default_tier, Some(ModelTier::Deep));
    }

    #[test]
    fn reviewer_defaults_to_deep_tier() {
        let p = all().into_iter().find(|p| p.name == "reviewer").unwrap();
        assert_eq!(p.default_tier, Some(ModelTier::Deep));
    }

    #[test]
    fn builtin_names_are_unique() {
        let names: std::collections::HashSet<&str> =
            BUILTIN_SOURCES.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names.len(),
            BUILTIN_SOURCES.len(),
            "built-in profile names must be unique"
        );
    }
}
