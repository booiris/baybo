//! Skills compiled into the cargo `[[bin]]` so a fresh workspace has
//! useful defaults the moment the gateway boots — no `baybo skills
//! install` required, no on-disk SKILL.md to lose track of.
//!
//! Each built-in lives in `crates/skills/src/builtin/<name>/SKILL.md`
//! and is embedded via `include_str!`. Parsing reuses
//! [`crate::loader::parse_skill_md`] so the YAML frontmatter contract
//! is identical to an agent's on-disk skills.
//!
//! Built-ins land in [`crate::SkillRegistry`] via
//! [`crate::SkillRegistry::register_builtins`]. They belong to the process
//! rather than to any persona, which is what makes
//! [`crate::UNIVERSAL_SKILLS`] safe to share; an agent whose own directory
//! carries the same name shadows one, inside that agent's scope only.

use baybo_model::{ArtifactSource, TrustLevel};

use crate::SkillDefinition;
use crate::loader::parse_skill_md;

/// Names of the runtime-reference skills. Referenced by
/// [`crate::registry::UNIVERSAL_SKILLS`] as well as the registration below,
/// so they are consts rather than repeated literals.
pub const BAYBO_CLI_SKILL_NAME: &str = "baybo-cli";
pub const BAYBO_HELP_SKILL_NAME: &str = "baybo-help";
const DECK_SKILL_NAME: &str = "deck";
const HTML_GEN_SKILL_NAME: &str = "html-gen";

const BAYBO_CLI_SKILL_MD: &str = include_str!("builtin/baybo-cli/SKILL.md");
const BAYBO_HELP_SKILL_MD: &str = include_str!("builtin/baybo-help/SKILL.md");
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
        (BAYBO_CLI_SKILL_NAME, BAYBO_CLI_SKILL_MD),
        (BAYBO_HELP_SKILL_NAME, BAYBO_HELP_SKILL_MD),
        (DECK_SKILL_NAME, DECK_SKILL_MD),
        (HTML_GEN_SKILL_NAME, HTML_GEN_SKILL_MD),
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
    fn deck_metadata_supports_model_and_slash_invocation_only_on_owner() {
        let skill = all()
            .into_iter()
            .find(|skill| skill.name == DECK_SKILL_NAME)
            .expect("deck builtin parses");
        let summary = crate::SkillSummary::from(&skill);

        assert!(skill.agent_invocable);
        assert_eq!(skill.command.as_deref(), Some(DECK_SKILL_NAME));
        assert!(skill.description.contains("ordinary language"));
        assert!(summary.allows_channel(&baybo_model::ChannelType::owner()));
        assert!(!summary.allows_channel(&baybo_model::ChannelType::telegram()));
    }

    #[test]
    fn baybo_help_is_an_invocable_source_backed_reference() {
        let skill = all()
            .into_iter()
            .find(|skill| skill.name == BAYBO_HELP_SKILL_NAME)
            .expect("baybo-help builtin parses");

        assert!(skill.agent_invocable);
        assert_eq!(skill.command.as_deref(), Some(BAYBO_HELP_SKILL_NAME));
        for tool in ["Bash", "Read", "Grep", "Glob"] {
            assert!(skill.allowed_tools.contains(&tool.to_string()), "{tool}");
        }
        assert!(
            skill
                .prompt_template
                .contains("https://github.com/booiris/baybo"),
            "the fallback repository must remain discoverable"
        );
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

    /// `html-gen` quotes the preview's Content-Security-Policy verbatim so
    /// the model can resolve cases the skill's own bullet list does not
    /// enumerate — "may I use a `<track>`?" is answered by `default-src
    /// 'none'` alone. That only holds while the quote is true, and a stale
    /// one is worse than the curated list it replaced: it reads as
    /// authoritative and is wrong in the direction the model trusts.
    ///
    /// The policy's one home is the Swift handler that sets the header.
    /// Reaching across into `app/ios` is unusual here, and deliberate: that
    /// tree is its own cargo workspace whose CI jobs are all `if: false`
    /// (see `/CLAUDE.md`), so a gate living there would never run. This one
    /// rides the root workspace's gating `cargo test`.
    ///
    /// Compared as a SET of directives, so the skill stays free to wrap the
    /// policy across lines for readability and neither side's trailing `;`
    /// matters.
    #[test]
    fn html_gen_quotes_the_preview_csp_the_ios_handler_actually_sends() {
        use std::collections::BTreeSet;

        const HANDLER: &str = "../../app/ios/App/Web/TranscriptSchemeHandler.swift";

        fn directives(csp: &str) -> BTreeSet<String> {
            csp.split(';')
                .map(|directive| directive.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|directive| !directive.is_empty())
                .collect()
        }

        let handler_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(HANDLER);
        let handler = std::fs::read_to_string(&handler_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", handler_path.display()));
        // Anchored on the DECLARATION, not the first mention: the identifier
        // also appears in prose (a `see htmlPreviewCSP` cross-reference above
        // the `/vendor/` path constant), and anchoring on that would compare
        // whichever literal happened to follow the comment.
        let (_, after) = handler
            .split_once("let htmlPreviewCSP")
            .expect("the handler declares htmlPreviewCSP");
        let open = after.find('"').expect("its value is a string literal");
        let close = open + 1 + after[open + 1..].find('"').expect("literal is closed");
        let served = &after[open + 1..close];

        let start = HTML_GEN_SKILL_MD
            .find("default-src")
            .expect("the skill quotes a policy");
        let end = start
            + HTML_GEN_SKILL_MD[start..]
                .find("```")
                .expect("the quote sits in a fenced block");
        let quoted = &HTML_GEN_SKILL_MD[start..end];

        assert_eq!(
            directives(quoted),
            directives(served),
            "html-gen/SKILL.md quotes a policy the handler no longer sends; \
             reconcile it with {}",
            handler_path.display()
        );
    }
}
