//! Tolerant extraction of a single JSON object from an LLM reply.
//!
//! Models told to "respond with JSON only" still wrap the body in
//! ```` ```json ```` fences or trail prose after it. [`extract_json_object`]
//! strips an optional fence and returns the first balanced `{…}` span so a
//! caller can `serde_json::from_str` it. Shared by every verdict-style
//! classifier (skill risk assessor, the Bash auto-mode risk judge) so the
//! brace-matching lives in one tested place.

/// Strip an optional opening/closing Markdown code fence (with or without a
/// language tag) and surrounding whitespace. Returns the input trimmed when
/// there is no fence.
fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // Optional language tag on the opening fence (`json`, `JSON`, `jsonc`…).
    let after_tag = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or(rest);
    after_tag.strip_suffix("```").unwrap_or(after_tag).trim()
}

/// Return the first balanced `{…}` object in `reply`, ignoring braces inside
/// string literals (so `{"k":"}"}` is handled). `None` when no complete object
/// is present. A leading code fence is stripped first.
pub fn extract_json_object(reply: &str) -> Option<&str> {
    let s = strip_fences(reply);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_object() {
        assert_eq!(extract_json_object(r#"{"a":1}"#), Some(r#"{"a":1}"#));
    }

    #[test]
    fn fenced_object() {
        assert_eq!(
            extract_json_object("```json\n{\"a\":1}\n```"),
            Some(r#"{"a":1}"#)
        );
    }

    #[test]
    fn trailing_prose_after_object() {
        assert_eq!(
            extract_json_object(r#"{"a":1}  also note this"#),
            Some(r#"{"a":1}"#)
        );
    }

    #[test]
    fn braces_inside_strings_dont_unbalance() {
        let raw = r#"{"k":"}{"}"#;
        assert_eq!(extract_json_object(raw), Some(raw));
    }

    #[test]
    fn no_object() {
        assert_eq!(extract_json_object("looks fine to me"), None);
    }

    #[test]
    fn unterminated_object() {
        assert_eq!(extract_json_object(r#"{"a":1"#), None);
    }
}
