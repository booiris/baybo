use aura_core::{ChatMessage, ContentBlock, Role};

/// Build a summary chat message from a summary text string.
///
/// The summary is injected as a system-level message so the LLM treats it
/// as authoritative context rather than a user turn.
pub fn summary_to_message(summary: &str) -> ChatMessage {
    ChatMessage {
        role: Role::System,
        content: vec![ContentBlock::Text(format!(
            "[Compressed summary of earlier conversation]\n{summary}"
        ))],
    }
}

/// Extract the plain text from a slice of chat messages, suitable for
/// passing to a summarization LLM or for token counting.
pub fn messages_to_text(messages: &[ChatMessage]) -> String {
    let mut parts = Vec::with_capacity(messages.len());
    for msg in messages {
        let role_label = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let text: String = msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("{role_label}: {text}"));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summary_to_message() {
        let msg = summary_to_message("Key facts: user wants X.");
        assert_eq!(msg.role, Role::System);
        match &msg.content[0] {
            ContentBlock::Text(t) => {
                assert!(t.contains("Compressed summary"));
                assert!(t.contains("Key facts: user wants X."));
            }
            _ => panic!("Expected text content"),
        }
    }

    #[test]
    fn test_messages_to_text() {
        let messages = vec![
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text("Hello".to_string())],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text("Hi there".to_string())],
            },
        ];
        let text = messages_to_text(&messages);
        assert!(text.contains("user: Hello"));
        assert!(text.contains("assistant: Hi there"));
    }
}
