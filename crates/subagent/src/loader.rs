//! Load a [`SubagentProfile`] from a single `<name>.md` file.
//!
//! A profile markdown file begins with a YAML frontmatter block fenced
//! by `---` and is followed by the system-prompt body. Frontmatter
//! tunes identity / discovery (`name`, `description`, `default_tier`,
//! `version`); the body becomes [`SubagentProfile::system_prompt`] and
//! is what REPLACES the parent's [`Soul`] inside the child actor.
//!
//! Parsing accepts the same YAML subset as `baybo-skills::loader`: flow
//! scalars (optionally quoted), `true`/`false`, inline `[a, b]` lists,
//! and block `- item` lists. Anything more exotic (anchors, folded
//! blocks, nested maps) is rejected so misreads don't slip through.
//!
//! Disk layout: `<workspace>/agents/<name>.md`. One file per profile —
//! no directory-per-profile ceremony like `baybo-skills` because a
//! profile has no linked-files concern (the prompt body alone IS the
//! whole thing).
//!
//! [`SubagentProfile`]: crate::SubagentProfile
//! [`Soul`]: https://example.invalid

use std::collections::BTreeMap;
use std::path::Path;

use baybo_model::{ArtifactSource, ModelTier, TrustLevel};

use crate::SubagentProfile;
use crate::validation::{normalize_line_endings, validate_profile_name, validate_profile_version};

/// Read `<name>.md` and build a [`SubagentProfile`] from it. The file
/// path is recorded on the definition so the upcoming risk assessor
/// can hash the prompt body alongside the manifest.
pub fn load_profile_from_file(path: &Path) -> Result<SubagentProfile, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let default_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mut profile = parse_profile_md(&content, &default_name)?;
    profile.source_path = Some(path.to_path_buf());
    Ok(profile)
}

/// Parse raw contents. `default_name` is used as `name` when the
/// frontmatter doesn't set one explicitly — the filename stem is the
/// default identifier.
pub fn parse_profile_md(content: &str, default_name: &str) -> Result<SubagentProfile, String> {
    let normalized = normalize_line_endings(content);
    let (frontmatter, body) = split_frontmatter(&normalized);
    let fm = match frontmatter {
        Some(text) => parse_yaml_subset(text)?,
        None => BTreeMap::new(),
    };

    let name = match fm.get("name") {
        Some(YamlValue::Scalar(s)) => s.clone(),
        Some(_) => return Err("`name` must be a scalar".into()),
        None => default_name.to_string(),
    };
    if name.is_empty() {
        return Err("subagent profile has no `name` and no filename stem".into());
    }
    if !validate_profile_name(&name) {
        return Err(format!(
            "subagent profile name '{name}' is invalid: must start with alphanumeric, then contain only [a-zA-Z0-9._-], 1-64 chars"
        ));
    }

    let description = read_scalar(&fm, "description").unwrap_or_default();
    if description.is_empty() {
        return Err(format!(
            "subagent profile '{name}' must declare a non-empty `description` (shown to the parent LLM when picking subagent_type)"
        ));
    }

    let default_tier = match read_scalar(&fm, "default_tier") {
        Some(label) => match ModelTier::parse(&label) {
            Some(tier) => Some(tier),
            None => {
                return Err(format!(
                    "subagent profile '{name}' has unknown default_tier '{label}'; expected fast|balanced|deep"
                ));
            }
        },
        None => None,
    };

    let version = read_scalar(&fm, "version").unwrap_or_else(|| "0.0.0".to_string());
    if !validate_profile_version(&version) {
        return Err(format!(
            "subagent profile version '{version}' is invalid: must be [a-zA-Z0-9._\\-+~]{{1,32}}"
        ));
    }

    let system_prompt = body.trim_start_matches('\n').trim_end().to_string();
    if system_prompt.is_empty() {
        return Err(format!(
            "subagent profile '{name}' has an empty body — the markdown after the frontmatter is the child actor's system prompt and cannot be blank"
        ));
    }

    Ok(SubagentProfile {
        name,
        version,
        description,
        system_prompt,
        default_tier,
        source: ArtifactSource::Workspace,
        trust_level: TrustLevel::Trusted,
        source_path: None,
    })
}

/// Split off a leading `---\n...\n---\n` frontmatter block, if present.
/// Returns `(Some(frontmatter_body), remainder_after_fence)` on match,
/// or `(None, original)` otherwise. A BOM is stripped if present.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(after_open) = text.strip_prefix("---") else {
        return (None, text);
    };
    let after_open = match after_open.strip_prefix('\n') {
        Some(rest) => rest,
        None => return (None, text),
    };

    let mut cursor = 0usize;
    while cursor < after_open.len() {
        let rest = &after_open[cursor..];
        let line_end = rest
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(after_open.len());
        let line = after_open[cursor..line_end].trim_end_matches('\r');
        if line == "---" {
            let body_start = if line_end < after_open.len() {
                line_end + 1
            } else {
                line_end
            };
            return (Some(&after_open[..cursor]), &after_open[body_start..]);
        }
        if line_end == after_open.len() {
            break;
        }
        cursor = line_end + 1;
    }
    (None, text)
}

#[derive(Debug, Clone)]
enum YamlValue {
    Scalar(String),
    /// Multi-line `|` literal block: lines collapsed into a single
    /// scalar (newlines preserved). Used so `description` can span
    /// paragraphs cleanly.
    LiteralBlock(String),
}

fn read_scalar(fm: &BTreeMap<String, YamlValue>, key: &str) -> Option<String> {
    match fm.get(key) {
        Some(YamlValue::Scalar(s)) if !s.is_empty() => Some(s.clone()),
        Some(YamlValue::LiteralBlock(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn parse_yaml_subset(text: &str) -> Result<BTreeMap<String, YamlValue>, String> {
    let mut out = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        if raw.starts_with(' ') || raw.starts_with('\t') {
            return Err(format!("unexpected indented line: {raw}"));
        }

        let Some(colon) = trimmed.find(':') else {
            return Err(format!("missing `:` in frontmatter line: {trimmed}"));
        };
        let key = trimmed[..colon].trim().to_string();
        let rest = trimmed[colon + 1..].trim();

        if rest == "|" {
            // YAML literal block: take every following indented line as
            // the block body, dedented by the shared indent. A blank
            // line ends the block.
            let (block, consumed) = collect_literal_block(&lines, i + 1)?;
            out.insert(key, YamlValue::LiteralBlock(block));
            i = consumed;
        } else if rest.is_empty() {
            return Err(format!(
                "unsupported frontmatter shape under `{key}`: only scalar and `|` literal blocks are accepted"
            ));
        } else if (rest.starts_with('[') && rest.ends_with(']'))
            || rest == "true"
            || rest == "false"
        {
            return Err(format!(
                "unsupported frontmatter value for `{key}`: lists and bools are not yet accepted (only scalars and `|` literal blocks)"
            ));
        } else {
            out.insert(key, YamlValue::Scalar(unquote(rest)));
            i += 1;
        }
    }
    Ok(out)
}

/// Consume a `|` literal block starting at `start`. Returns the block
/// body (newline-joined, common indent stripped) and the next line
/// index to resume top-level parsing from. Block terminates on the
/// first non-indented non-blank line.
fn collect_literal_block(lines: &[&str], start: usize) -> Result<(String, usize), String> {
    let mut body = String::new();
    let mut indent: Option<usize> = None;
    let mut i = start;
    while i < lines.len() {
        let li = lines[i];
        if li.trim().is_empty() {
            body.push('\n');
            i += 1;
            continue;
        }
        let leading: usize = li.bytes().take_while(|b| *b == b' ').count();
        if leading == 0 {
            break;
        }
        let dedent = *indent.get_or_insert(leading);
        if leading < dedent {
            return Err(format!(
                "indented literal-block line drops below its opening indent: {li}"
            ));
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&li[dedent..]);
        i += 1;
    }
    Ok((body.trim_end_matches('\n').to_string(), i))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_profile() {
        let src = r#"---
name: general-purpose
description: catch-all subagent for ad-hoc tasks
---
You are a helpful subagent. Investigate the task and report concisely.
"#;
        let p = parse_profile_md(src, "fallback").unwrap();
        assert_eq!(p.name, "general-purpose");
        assert_eq!(p.description, "catch-all subagent for ad-hoc tasks");
        assert!(p.system_prompt.contains("You are a helpful subagent"));
        assert_eq!(p.version, "0.0.0");
        assert!(p.default_tier.is_none());
    }

    #[test]
    fn name_defaults_to_filename_stem() {
        let src = "---\ndescription: defaults test\n---\nbody\n";
        let p = parse_profile_md(src, "my-profile").unwrap();
        assert_eq!(p.name, "my-profile");
    }

    #[test]
    fn default_tier_parses_known_label() {
        let src = "---\nname: planner\ndescription: plans\ndefault_tier: deep\n---\nbody\n";
        let p = parse_profile_md(src, "planner").unwrap();
        assert_eq!(p.default_tier, Some(ModelTier::Deep));
    }

    #[test]
    fn unknown_default_tier_is_rejected() {
        let src = "---\nname: p\ndescription: d\ndefault_tier: ultradeep\n---\nbody\n";
        assert!(parse_profile_md(src, "p").is_err());
    }

    #[test]
    fn empty_description_is_rejected() {
        let src = "---\nname: p\n---\nbody\n";
        assert!(parse_profile_md(src, "p").is_err());
    }

    #[test]
    fn empty_body_is_rejected() {
        let src = "---\nname: p\ndescription: d\n---\n\n   \n";
        assert!(parse_profile_md(src, "p").is_err());
    }

    #[test]
    fn invalid_name_is_rejected() {
        let src = "---\nname: has spaces\ndescription: d\n---\nbody\n";
        assert!(parse_profile_md(src, "fallback").is_err());
    }

    #[test]
    fn body_without_frontmatter_falls_back_but_requires_description() {
        let src = "Plain markdown body\n";
        let err = parse_profile_md(src, "plain").unwrap_err();
        assert!(err.contains("description"));
    }

    #[test]
    fn body_is_trimmed_of_leading_and_trailing_blank_lines() {
        let src = "---\nname: p\ndescription: d\n---\n\n\nactual prompt body\n\n\n";
        let p = parse_profile_md(src, "p").unwrap();
        assert_eq!(p.system_prompt, "actual prompt body");
    }

    #[test]
    fn quoted_scalar_is_unquoted() {
        let src = "---\nname: p\ndescription: \"has: colon inside\"\n---\nbody\n";
        let p = parse_profile_md(src, "p").unwrap();
        assert_eq!(p.description, "has: colon inside");
    }

    #[test]
    fn literal_block_description_preserves_newlines_and_dedents() {
        let src = r#"---
name: p
description: |
  first line
  second line

  fourth after blank
---
body
"#;
        let p = parse_profile_md(src, "p").unwrap();
        assert_eq!(
            p.description,
            "first line\nsecond line\n\nfourth after blank"
        );
    }

    #[test]
    fn list_in_frontmatter_is_rejected_until_supported() {
        let src = "---\nname: p\ndescription: d\naliases: [a, b]\n---\nbody\n";
        let err = parse_profile_md(src, "p").unwrap_err();
        assert!(err.contains("lists and bools"));
    }
}
