use serde::Deserialize;

use crate::error::CodeBuilderError;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawPlan {
    pub code: String,
    #[serde(default)]
    pub network_required: bool,
    /// LLM's one-sentence justification for needing the network.
    /// Required (and surfaced in the approval prompt) when
    /// `network_required` is true; ignored otherwise.
    #[serde(default)]
    pub network_reason: Option<String>,
    #[serde(default)]
    pub readable_paths: Vec<String>,
    /// Absolute paths the script will write to in addition to its
    /// scratch CWD. Subset of the caller's `extra_writable_paths`.
    /// Empty if the script only writes inside its CWD.
    #[serde(default)]
    pub writable_paths: Vec<String>,
    #[serde(default)]
    pub estimated_runtime_seconds: Option<u64>,
    #[serde(default)]
    pub estimated_memory_mb: Option<u64>,
    #[serde(default)]
    pub rationale: String,
}

pub(crate) fn parse_plan(raw: &str) -> Result<RawPlan, CodeBuilderError> {
    let trimmed = strip_fences(raw.trim());
    let obj =
        extract_first_json_object(trimmed).ok_or_else(|| CodeBuilderError::LlmReturnedNoJson {
            preview: preview(raw),
        })?;
    let parsed: RawPlan = serde_json::from_str(obj)?;
    Ok(parsed)
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let after_tag = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or(rest);
    after_tag.strip_suffix("```").unwrap_or(after_tag).trim()
}

fn extract_first_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn preview(raw: &str) -> String {
    const MAX: usize = 200;
    let trimmed = raw.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &trimmed[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_plan() {
        let raw = r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"trivial"}"#;
        let p = parse_plan(raw).unwrap();
        assert_eq!(p.code, "print(1)");
        assert!(!p.network_required);
        assert_eq!(p.estimated_runtime_seconds, Some(1));
        assert_eq!(p.estimated_memory_mb, Some(64));
    }

    #[test]
    fn tolerates_fenced_json() {
        let raw = "```json\n{\"code\":\"print(1)\"}\n```";
        let p = parse_plan(raw).unwrap();
        assert_eq!(p.code, "print(1)");
    }

    #[test]
    fn tolerates_trailing_prose() {
        let raw = r#"sure, here you go: {"code":"print(1)"} hope that helps"#;
        let p = parse_plan(raw).unwrap();
        assert_eq!(p.code, "print(1)");
    }

    #[test]
    fn rejects_non_json_response() {
        let err = parse_plan("totally not json").unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmReturnedNoJson { .. }));
    }

    #[test]
    fn rejects_missing_code_field() {
        let err = parse_plan(r#"{"network_required":false}"#).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanInvalid(_)));
    }

    #[test]
    fn defaults_for_optional_fields() {
        let p = parse_plan(r#"{"code":"x"}"#).unwrap();
        assert!(p.readable_paths.is_empty());
        assert!(!p.network_required);
        assert_eq!(p.estimated_runtime_seconds, None);
        assert_eq!(p.estimated_memory_mb, None);
        assert!(p.rationale.is_empty());
    }
}
