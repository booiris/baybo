//! Tool-name encoding for the LLM wire boundary.
//!
//! Baybo registers MCP tools as `<server>/<tool>`, but every supported
//! provider rejects `/` in `tools[].name`:
//!
//!   * OpenAI / Anthropic — `^[a-zA-Z0-9_-]{1,64}$`
//!   * Gemini — `^[a-zA-Z_][a-zA-Z0-9_.:-]{0,127}$`
//!   * OpenAI Codex Responses — `^[a-zA-Z0-9_-]+$`
//!
//! Encode `/` → `__` on the way out and reverse on the way back so the
//! agent loop keeps using the original namespaced name internally
//! while the wire stays valid. Any other forbidden char is replaced
//! with `_` (lossy — round-trip fails — but at least the request goes
//! through and the model's tool call surfaces as a clean
//! `ToolNotFound` rather than a 400 from the provider).

pub(crate) fn sanitize_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for ch in name.chars() {
        match ch {
            '/' => out.push_str("__"),
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => out.push(c),
            _ => out.push('_'),
        }
    }
    out
}

pub(crate) fn unsanitize_tool_name(name: &str) -> String {
    name.replace("__", "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_round_trips() {
        let original = "browser/navigate_page";
        let sanitized = sanitize_tool_name(original);
        assert!(
            sanitized
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "sanitized name must satisfy the strictest provider regex: {sanitized}",
        );
        assert_eq!(unsanitize_tool_name(&sanitized), original);
    }

    #[test]
    fn sanitize_is_idempotent_on_already_clean_names() {
        // Applying twice must not corrupt names — the rig path may
        // sanitize before handing off to a provider that sanitizes
        // again (Codex Responses), and inbound unsanitization may
        // run on a name that was never sanitized.
        let original = "update_profile";
        assert_eq!(sanitize_tool_name(original), original);
        assert_eq!(sanitize_tool_name(&sanitize_tool_name(original)), original,);
    }

    #[test]
    fn single_underscores_survive_unsanitize() {
        // The reverse mapping replaces only `__`; tool names with one
        // underscore (the common case) must come back intact.
        assert_eq!(unsanitize_tool_name("update_profile"), "update_profile");
    }

    #[test]
    fn unsupported_chars_become_underscore() {
        // Lossy fallback for chars no provider accepts; we accept the
        // round-trip break in exchange for not 400-ing the request.
        assert_eq!(sanitize_tool_name("a@b!c"), "a_b_c");
    }
}
