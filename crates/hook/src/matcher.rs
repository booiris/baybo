use regex::Regex;

use crate::error::HookError;

/// Determines when a hook fires based on the event's matcher target.
///
/// Matchers are auto-detected from the pattern string:
/// - `*`, `""`, or omitted → `All` (match everything)
/// - Only alphanumeric + `_` + `|` → `Exact` or `OneOf`
/// - Any other character → `Regex`
#[derive(Debug, Clone)]
pub enum HookMatcher {
    /// Matches every occurrence of the event.
    All,
    /// Matches a single exact string.
    Exact(String),
    /// Matches any of the `|`-separated strings.
    OneOf(Vec<String>),
    /// Matches a regex pattern against the target.
    Pattern(Regex),
}

impl HookMatcher {
    /// Parse a pattern string into a `HookMatcher`.
    ///
    /// Detection rules:
    /// - `"*"`, `""` → `All`
    /// - Only `[A-Za-z0-9_|]` chars → split on `|` for `Exact` / `OneOf`
    /// - Anything else → compile as `Regex`
    pub fn parse(pattern: &str) -> Result<Self, HookError> {
        let trimmed = pattern.trim();
        if trimmed.is_empty() || trimmed == "*" {
            return Ok(Self::All);
        }

        let is_simple = trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|');

        if is_simple {
            let parts: Vec<String> = trimmed.split('|').map(|s| s.to_string()).collect();
            if parts.len() == 1 {
                Ok(Self::Exact(parts.into_iter().next().unwrap_or_default()))
            } else {
                Ok(Self::OneOf(parts))
            }
        } else {
            let regex =
                Regex::new(trimmed).map_err(|e| HookError::InvalidMatcher(e.to_string()))?;
            Ok(Self::Pattern(regex))
        }
    }

    /// Test whether this matcher accepts the given target string.
    pub fn matches(&self, target: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(expected) => target == expected,
            Self::OneOf(options) => options.iter().any(|opt| opt == target),
            Self::Pattern(regex) => regex.is_match(target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_matcher_from_empty() {
        let m = HookMatcher::parse("").unwrap();
        assert!(m.matches("anything"));
        assert!(m.matches(""));
    }

    #[test]
    fn all_matcher_from_star() {
        let m = HookMatcher::parse("*").unwrap();
        assert!(m.matches("Bash"));
        assert!(m.matches("Edit"));
    }

    #[test]
    fn exact_matcher() {
        let m = HookMatcher::parse("Bash").unwrap();
        assert!(m.matches("Bash"));
        assert!(!m.matches("Edit"));
        assert!(!m.matches("bash"));
    }

    #[test]
    fn one_of_matcher() {
        let m = HookMatcher::parse("Edit|Write").unwrap();
        assert!(m.matches("Edit"));
        assert!(m.matches("Write"));
        assert!(!m.matches("Bash"));
    }

    #[test]
    fn regex_matcher() {
        let m = HookMatcher::parse("^ext__.*").unwrap();
        assert!(m.matches("ext__memory__create"));
        assert!(m.matches("ext__fs__read"));
        assert!(!m.matches("builtin__bash"));
    }

    #[test]
    fn regex_matcher_complex() {
        let m = HookMatcher::parse("ext__.*__write.*").unwrap();
        assert!(m.matches("ext__fs__write_file"));
        assert!(m.matches("ext__memory__write"));
        assert!(!m.matches("ext__fs__read"));
    }

    #[test]
    fn invalid_regex_returns_error() {
        let result = HookMatcher::parse("[invalid");
        assert!(result.is_err());
    }

    #[test]
    fn underscore_in_simple_pattern() {
        let m = HookMatcher::parse("file_read").unwrap();
        assert!(m.matches("file_read"));
        assert!(!m.matches("file_write"));
    }

    #[test]
    fn pipe_separated_with_underscores() {
        let m = HookMatcher::parse("file_read|file_write|bash").unwrap();
        assert!(m.matches("file_read"));
        assert!(m.matches("file_write"));
        assert!(m.matches("bash"));
        assert!(!m.matches("edit"));
    }
}
