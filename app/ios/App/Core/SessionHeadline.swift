import Foundation

/// What a conversation is CALLED on this device.
///
/// One rule, two readers: the chat list's bold first line and the search
/// result card. They must agree — a conversation whose title pass has not run
/// yet is named by a snippet of its last user message in the list, and a search
/// card that fell back to "Untitled" instead would be showing the reader a
/// different name for a row they are looking at two screens away.
///
/// The search response deliberately does not carry `last_user_text`, so this is
/// answerable only from the local row. A card with no local row (a conversation
/// started elsewhere and not yet synced here) falls back to the server title.
enum SessionHeadline {
    /// Longest user-message snippet a headline shows before the title pass has
    /// run — a compact title stand-in, not the full message.
    static let maxChars = 8

    /// The title, else a short snippet of the last user message, else `""` for a
    /// session with no user turn yet. Empty is returned rather than a
    /// placeholder because the two call sites word that case differently.
    static func text(title: String?, userText: String?) -> String {
        if let title, !title.isEmpty { return title }
        if let userText, !userText.isEmpty { return snippet(userText) }
        return ""
    }

    /// Whitespace collapsed and clipped to `maxChars`, with a trailing ellipsis
    /// when it overflows.
    static func snippet(_ text: String) -> String {
        let collapsed = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return collapsed.count > maxChars
            ? String(collapsed.prefix(maxChars)) + "…"
            : collapsed
    }
}
