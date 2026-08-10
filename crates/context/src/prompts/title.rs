//! Prompt + response sanitizing for the conversation-title pass.

use baybo_model::MAX_SESSION_TITLE_LEN;

const PROMPT_TEMPLATE: &str = r#"You are titling a brand-new conversation from the user's first message. Write a short, specific title that captures what the user is asking about — the key subject, not filler.

Rules:
- At most 8 words. Prefer 3-6. A noun phrase, not a sentence.
- Write the title in the SAME language as the user's message.
- Capture the concrete topic (names, tools, errors, tasks the user mentions), not a generic category.
- No surrounding quotes, no trailing punctuation, no prefix like "Title:", no emoji.
- Output ONLY the title text on a single line. Nothing else.

User's first message:
<user_message>
{{question}}
</user_message>"#;

pub fn build_title_prompt(question: &str) -> String {
    PROMPT_TEMPLATE.replace("{{question}}", question)
}

/// Normalize a raw model reply into a storable title.
pub fn sanitize_title(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;

    let line = strip_label(line);

    let line = strip_wrapping_quotes(line.trim());

    let trimmed = line
        .trim()
        .trim_end_matches(['.', '。', '!', '！', '?', '？', ',', '，']);

    let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }

    Some(cap_chars(&collapsed, MAX_SESSION_TITLE_LEN))
}

fn strip_label(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    for label in ["title:", "标题:", "标题："] {
        if lower.starts_with(label) || s.starts_with(label) {
            return s[label.len()..].trim_start();
        }
    }
    s
}

fn strip_wrapping_quotes(s: &str) -> &str {
    let pairs = [
        ('"', '"'),
        ('\'', '\''),
        ('“', '”'),
        ('「', '」'),
        ('『', '』'),
    ];
    for (open, close) in pairs {
        if let Some(inner) = s.strip_prefix(open).and_then(|r| r.strip_suffix(close)) {
            return inner;
        }
    }
    s
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let hard: String = s.chars().take(max).collect();
    match hard.rsplit_once(char::is_whitespace) {
        Some((head, _)) if !head.is_empty() => head.trim_end().to_string(),
        _ => hard.trim_end().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_substitutes_question_and_drops_placeholder() {
        let p = build_title_prompt("how do I reset my password?");
        assert!(p.contains("how do I reset my password?"));
        assert!(!p.contains("{{question}}"));
        assert!(p.contains("<user_message>"));
    }

    #[test]
    fn sanitize_trims_quotes_labels_and_punctuation() {
        assert_eq!(
            sanitize_title("\"Reset password flow.\"").as_deref(),
            Some("Reset password flow")
        );
        assert_eq!(
            sanitize_title("Title: Fix login redirect").as_deref(),
            Some("Fix login redirect")
        );
        assert_eq!(
            sanitize_title("  Deploy to Kubernetes  ").as_deref(),
            Some("Deploy to Kubernetes")
        );
    }

    #[test]
    fn sanitize_takes_first_nonempty_line() {
        assert_eq!(
            sanitize_title("\n\nMigrate DB schema\nThis title captures…").as_deref(),
            Some("Migrate DB schema")
        );
    }

    #[test]
    fn sanitize_rejects_empty_or_punctuation_only() {
        assert_eq!(sanitize_title(""), None);
        assert_eq!(sanitize_title("   \n  "), None);
        assert_eq!(sanitize_title("..."), None);
    }

    #[test]
    fn sanitize_preserves_non_ascii_and_collapses_whitespace() {
        assert_eq!(
            sanitize_title("「登录重定向 bug 修复」").as_deref(),
            Some("登录重定向 bug 修复")
        );
        assert_eq!(
            sanitize_title("Fix    the   flaky\ttest").as_deref(),
            Some("Fix the flaky test")
        );
    }

    #[test]
    fn sanitize_caps_length_on_word_boundary() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho";
        let out = sanitize_title(long).unwrap();
        assert!(out.chars().count() <= MAX_SESSION_TITLE_LEN, "got {out:?}");
        assert!(!out.ends_with(' '));
        assert!(long.starts_with(&out));
    }

    #[test]
    fn sanitize_hard_cuts_a_single_overlong_token() {
        let token = "a".repeat(MAX_SESSION_TITLE_LEN + 20);
        let out = sanitize_title(&token).unwrap();
        assert_eq!(out.chars().count(), MAX_SESSION_TITLE_LEN);
    }
}
