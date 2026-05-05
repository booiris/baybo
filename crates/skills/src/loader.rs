//! Load skills from `<skills_dir>/<skill-name>/SKILL.md` files.
//!
//! A SKILL.md file begins with YAML frontmatter between `---` fences and is
//! followed by a Markdown body. The body becomes `prompt_template`; the
//! frontmatter tunes invocation (`name`, `description`, `when_to_use`,
//! `allowed-tools`, `disable-model-invocation`, `user-invocable`,
//! `argument-hint`, `version`).
//!
//! Parsing deliberately supports only the YAML subset that appears in
//! real SKILL.md files: flow scalars (optionally quoted), `true`/`false`,
//! inline `[a, b]` lists, and block `- item` lists. Full-YAML features
//! such as anchors or folded/literal blocks aren't needed here and are
//! rejected rather than silently mis-parsed.

use std::collections::BTreeMap;
use std::path::Path;

use aura_model::{ArtifactSource, TrustLevel};
use tracing::warn;

use crate::validation::{normalize_line_endings, validate_skill_name, validate_skill_version};
use crate::{LinkedFiles, SkillDefinition, SkillRequirements, linked_files};

/// Read `<dir>/SKILL.md` and build a `SkillDefinition` from it. The
/// canonical directory path is recorded on the definition so downstream
/// consumers (e.g., the risk assessor) can hash the whole skill tree.
pub fn load_skill_from_dir(dir: &Path) -> Result<SkillDefinition, String> {
    let skill_md = dir.join("SKILL.md");
    let content = std::fs::read_to_string(&skill_md)
        .map_err(|e| format!("reading {}: {e}", skill_md.display()))?;
    let default_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mut skill = parse_skill_md(&content, &default_name)?;
    skill.source_path = Some(dir.to_path_buf());
    skill.linked_files = match linked_files::enumerate(dir) {
        Ok(lf) => lf,
        Err(err) => {
            warn!(skill = %skill.name, dir = %dir.display(), error = %err, "linked-file enumeration failed; loading with empty inventory");
            LinkedFiles::default()
        }
    };
    Ok(skill)
}

/// Parse the raw contents of a SKILL.md file. `default_name` is used as
/// `name` when the frontmatter doesn't set one explicitly (the directory
/// name is the default skill identifier).
pub fn parse_skill_md(content: &str, default_name: &str) -> Result<SkillDefinition, String> {
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
        return Err("skill has no `name` and no parent directory name".into());
    }
    if !validate_skill_name(&name) {
        return Err(format!(
            "skill name '{name}' is invalid: must start with alphanumeric, then contain only [a-zA-Z0-9._-], 1-64 chars"
        ));
    }

    // description + when_to_use are concatenated so selection sees both.
    let description = read_scalar(&fm, "description").unwrap_or_default();
    let when_to_use = read_scalar(&fm, "when_to_use");
    let description = match (description.is_empty(), when_to_use) {
        (true, Some(w)) => w,
        (false, Some(w)) => format!("{description}\n\n{w}"),
        (_, None) => description,
    };

    let allowed_tools = match fm.get("allowed-tools") {
        Some(YamlValue::List(items)) => items.clone(),
        Some(YamlValue::Scalar(s)) => s.split_whitespace().map(String::from).collect(),
        Some(YamlValue::Bool(_)) => return Err("`allowed-tools` must be a list or string".into()),
        None => Vec::new(),
    };

    let disable_model_invocation = read_bool(&fm, "disable-model-invocation")?.unwrap_or(false);
    let user_invocable = read_bool(&fm, "user-invocable")?.unwrap_or(true);

    let command = if user_invocable {
        Some(name.clone())
    } else {
        None
    };
    let agent_invocable = !disable_model_invocation;

    let argument_hint = read_scalar(&fm, "argument-hint");
    let version = read_scalar(&fm, "version").unwrap_or_else(|| "0.0.0".to_string());
    if !validate_skill_version(&version) {
        return Err(format!(
            "skill version '{version}' is invalid: must be [a-zA-Z0-9._\\-+~]{{1,32}} (no whitespace, quotes, or angle brackets)"
        ));
    }

    Ok(SkillDefinition {
        name,
        version,
        description,
        command,
        agent_invocable,
        argument_hint,
        prompt_template: body.trim_start_matches('\n').to_string(),
        allowed_tools,
        source: ArtifactSource::Workspace,
        trust_level: TrustLevel::Trusted,
        requirements: SkillRequirements::default(),
        token_budget_hint: 2048,
        source_path: None,
        linked_files: LinkedFiles::default(),
    })
}

/// Split off a leading `---\n...\n---\n` frontmatter block, if present.
/// Returns `(Some(frontmatter_body), remainder_after_fence)` on match, or
/// `(None, original)` otherwise. A BOM is stripped if present.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(after_open) = text.strip_prefix("---") else {
        return (None, text);
    };
    // Require a newline right after the opening `---` so it's on its own line.
    let after_open = match after_open.strip_prefix('\n') {
        Some(rest) => rest,
        None => return (None, text),
    };

    // Find a closing `---` that sits alone on a line.
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
    Bool(bool),
    List(Vec<String>),
}

fn read_scalar(fm: &BTreeMap<String, YamlValue>, key: &str) -> Option<String> {
    match fm.get(key) {
        Some(YamlValue::Scalar(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn read_bool(fm: &BTreeMap<String, YamlValue>, key: &str) -> Result<Option<bool>, String> {
    match fm.get(key) {
        Some(YamlValue::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("`{key}` must be true or false")),
        None => Ok(None),
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
        // Top-level keys must start at column 0 — indented lines here are
        // either list items (handled below after their key) or an unsupported
        // nested mapping we reject rather than silently drop.
        if raw.starts_with(' ') || raw.starts_with('\t') {
            return Err(format!("unexpected indented line: {raw}"));
        }

        let Some(colon) = trimmed.find(':') else {
            return Err(format!("missing `:` in frontmatter line: {trimmed}"));
        };
        let key = trimmed[..colon].trim().to_string();
        let rest = trimmed[colon + 1..].trim();

        if rest.is_empty() {
            // Expect a block list on following indented lines.
            let mut items = Vec::new();
            i += 1;
            while i < lines.len() {
                let li = lines[i];
                if li.trim().is_empty() {
                    i += 1;
                    continue;
                }
                if !li.starts_with(' ') && !li.starts_with('\t') {
                    break;
                }
                let stripped = li.trim_start();
                let Some(item) = stripped.strip_prefix("- ") else {
                    return Err(format!("expected `- item` under `{key}`, got: {li}"));
                };
                items.push(unquote(item.trim()));
                i += 1;
            }
            out.insert(key, YamlValue::List(items));
        } else if rest.starts_with('[') && rest.ends_with(']') {
            let inner = &rest[1..rest.len() - 1];
            let items: Vec<String> = inner
                .split(',')
                .map(|s| unquote(s.trim()))
                .filter(|s| !s.is_empty())
                .collect();
            out.insert(key, YamlValue::List(items));
            i += 1;
        } else if rest == "true" || rest == "false" {
            out.insert(key, YamlValue::Bool(rest == "true"));
            i += 1;
        } else {
            out.insert(key, YamlValue::Scalar(unquote(rest)));
            i += 1;
        }
    }
    Ok(out)
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
    fn parses_minimal_frontmatter() {
        let src = "---\nname: greet\ndescription: say hi\n---\nHello world\n";
        let skill = parse_skill_md(src, "fallback").unwrap();
        assert_eq!(skill.name, "greet");
        assert_eq!(skill.description, "say hi");
        assert_eq!(skill.prompt_template, "Hello world\n");
        assert_eq!(skill.command.as_deref(), Some("greet"));
        assert!(skill.agent_invocable);
    }

    #[test]
    fn name_defaults_to_dir_when_missing() {
        let src = "---\ndescription: fallback-name test\n---\nbody";
        let skill = parse_skill_md(src, "mydir").unwrap();
        assert_eq!(skill.name, "mydir");
    }

    #[test]
    fn description_appends_when_to_use() {
        let src = "---\nname: s\ndescription: what it does\nwhen_to_use: triggers A or B\n---\n";
        let skill = parse_skill_md(src, "s").unwrap();
        assert!(skill.description.contains("what it does"));
        assert!(skill.description.contains("triggers A or B"));
    }

    #[test]
    fn allowed_tools_accepts_space_separated_string() {
        let src = "---\nname: s\nallowed-tools: Read Grep Bash\n---\n";
        let skill = parse_skill_md(src, "s").unwrap();
        assert_eq!(skill.allowed_tools, vec!["Read", "Grep", "Bash"]);
    }

    #[test]
    fn allowed_tools_accepts_block_list() {
        let src = "---\nname: s\nallowed-tools:\n  - Read\n  - Bash(git *)\n---\n";
        let skill = parse_skill_md(src, "s").unwrap();
        assert_eq!(skill.allowed_tools, vec!["Read", "Bash(git *)"]);
    }

    #[test]
    fn allowed_tools_accepts_inline_list() {
        let src = "---\nname: s\nallowed-tools: [Read, Grep]\n---\n";
        let skill = parse_skill_md(src, "s").unwrap();
        assert_eq!(skill.allowed_tools, vec!["Read", "Grep"]);
    }

    #[test]
    fn disable_model_invocation_clears_agent_flag() {
        let src = "---\nname: s\ndisable-model-invocation: true\n---\n";
        let skill = parse_skill_md(src, "s").unwrap();
        assert!(!skill.agent_invocable);
        // Still user-invocable by default.
        assert_eq!(skill.command.as_deref(), Some("s"));
    }

    #[test]
    fn user_invocable_false_removes_command() {
        let src = "---\nname: s\nuser-invocable: false\n---\n";
        let skill = parse_skill_md(src, "s").unwrap();
        assert!(skill.command.is_none());
        assert!(skill.agent_invocable);
    }

    #[test]
    fn quoted_scalar_is_unquoted() {
        let src = "---\nname: s\ndescription: \"has: colon inside\"\n---\n";
        let skill = parse_skill_md(src, "s").unwrap();
        assert_eq!(skill.description, "has: colon inside");
    }

    #[test]
    fn body_without_frontmatter_is_preserved() {
        let src = "Plain markdown body\n";
        let skill = parse_skill_md(src, "plain").unwrap();
        assert_eq!(skill.name, "plain");
        assert_eq!(skill.prompt_template, "Plain markdown body\n");
    }

    #[test]
    fn unterminated_frontmatter_is_treated_as_body() {
        // No closing `---`, so the whole thing is body and `name` falls back
        // to the directory default.
        let src = "---\nname: s\n# never closes\n";
        let skill = parse_skill_md(src, "fallback").unwrap();
        assert_eq!(skill.name, "fallback");
    }

    #[test]
    fn bad_bool_value_is_rejected() {
        let src = "---\nname: s\ndisable-model-invocation: yes\n---\n";
        // "yes" is treated as a scalar, not a bool, so read_bool returns Err.
        assert!(parse_skill_md(src, "s").is_err());
    }
}
