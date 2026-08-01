//! Skills compiled into the cargo `[[bin]]` so a fresh workspace has
//! useful defaults the moment the gateway boots — no `baybo skills
//! install` required, no on-disk SKILL.md to lose track of.
//!
//! Each built-in lives in `crates/skills/src/builtin/<name>/SKILL.md`
//! and is embedded via `include_str!`. Parsing reuses
//! [`crate::loader::parse_skill_md`] so the YAML frontmatter contract
//! is identical to workspace skills.
//!
//! Built-ins land in [`crate::SkillRegistry`] via
//! [`crate::SkillRegistry::register_builtins`]; workspace skills loaded
//! later with the same name override them, so an operator can always
//! patch shipped behaviour locally.

use baybo_model::{ArtifactSource, TrustLevel};

use crate::SkillDefinition;
use crate::loader::parse_skill_md;

const BAYBO_CLI_SKILL_MD: &str = include_str!("builtin/baybo-cli/SKILL.md");
const DECK_SKILL_MD: &str = include_str!("builtin/deck/SKILL.md");
const HTML_GEN_SKILL_MD: &str = include_str!("builtin/html-gen/SKILL.md");

/// Template token in built-in SKILL.md bodies that gets substituted
/// with the absolute path of the running `baybo` binary at register
/// time. The skill body uses `{{BAYBO_BIN}} status --live` instead of
/// bare `baybo status --live` so the agent's `Bash` tool can invoke
/// the CLI even when `baybo` isn't on `$PATH` (the common case in dev
/// builds and in detached deployments).
const BAYBO_BIN_TOKEN: &str = "{{BAYBO_BIN}}";

/// Every skill that ships with the binary. Order is irrelevant —
/// [`crate::SkillRegistry::register`] is keyed by `name`.
pub(crate) fn all() -> Vec<SkillDefinition> {
    let bin = resolve_baybo_bin();
    let raw = [
        ("baybo-cli", BAYBO_CLI_SKILL_MD),
        ("deck", DECK_SKILL_MD),
        ("html-gen", HTML_GEN_SKILL_MD),
    ];
    raw.into_iter()
        .filter_map(|(name, md)| parse(name, md, &bin))
        .collect()
}

/// Absolute path to the running binary, POSIX-quoted for safe
/// embedding in `sh -c` invocations. Falls back to the bare
/// [`baybo_workspace::paths::BIN_NAME`] when `current_exe()` errors
/// (rare — only happens if the binary file was moved or deleted
/// since exec) so the skill body stays usable; in that case the
/// agent still gets the right command, just relying on `$PATH`.
fn resolve_baybo_bin() -> String {
    match std::env::current_exe() {
        Ok(path) => sh_quote(&path.to_string_lossy()),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "current_exe() failed; built-in skill will reference bare {} on $PATH",
                baybo_workspace::paths::BIN_NAME,
            );
            baybo_workspace::paths::BIN_NAME.to_string()
        }
    }
}

/// Wrap in `'…'` and escape inner single quotes with the standard
/// `'\''` close/escape/reopen idiom. Safe for any path the FS can
/// produce. Mirrors `baybo_tools::builtin::bash::sh_quote`; copied
/// here so `baybo-skills` doesn't gain a dependency on `baybo-tools`.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Parse a built-in SKILL.md, substitute the absolute-bin token,
/// fix provenance fields, and return it.
///
/// Built-ins are `ArtifactSource::Inline` (no on-disk path to point at)
/// and `TrustLevel::Trusted` — they're part of the binary itself, so
/// they're as trusted as any other shipped code.
fn parse(name: &str, md: &str, bin: &str) -> Option<SkillDefinition> {
    match parse_skill_md(md, name) {
        Ok(mut skill) => {
            skill.prompt_template = skill.prompt_template.replace(BAYBO_BIN_TOKEN, bin);
            skill.source = ArtifactSource::Inline;
            skill.trust_level = TrustLevel::Trusted;
            skill.source_path = None;
            skill.linked_files = Default::default();
            Some(skill)
        }
        Err(e) => {
            tracing::warn!(skill = name, error = %e, "built-in SKILL.md failed to parse");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baybo_bin_token_is_substituted_in_prompt_template() {
        let bin = sh_quote("/fake/bin/baybo");
        let skill = parse(
            "baybo-cli",
            "---\nname: baybo-cli\ndescription: t\n---\nRun {{BAYBO_BIN}} status --live.",
            &bin,
        )
        .expect("test skill parses");
        assert!(
            skill.prompt_template.contains("'/fake/bin/baybo'"),
            "expected absolute bin path in body, got: {}",
            skill.prompt_template
        );
        assert!(
            !skill.prompt_template.contains(BAYBO_BIN_TOKEN),
            "token should be fully substituted, got: {}",
            skill.prompt_template
        );
    }

    #[test]
    fn sh_quote_escapes_internal_single_quotes() {
        assert_eq!(sh_quote("/a/b"), "'/a/b'");
        assert_eq!(sh_quote("/a's/b"), "'/a'\\''s/b'");
    }

    #[test]
    fn html_gen_metadata_is_visible_only_on_owner() {
        let skill = all()
            .into_iter()
            .find(|skill| skill.name == "html-gen")
            .expect("html-gen builtin parses");
        let summary = crate::SkillSummary::from(&skill);

        assert!(skill.agent_invocable);
        assert_eq!(skill.command.as_deref(), Some("html-gen"));
        assert!(skill.allowed_tools.contains(&"PutBlob".to_string()));
        assert!(summary.allows_channel(&baybo_model::ChannelType::owner()));
        assert!(!summary.allows_channel(&baybo_model::ChannelType::telegram()));
    }
}
