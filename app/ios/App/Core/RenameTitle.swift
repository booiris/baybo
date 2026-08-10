import Foundation

/// Conversation-rename rules, kept out of the dialog so they can be tested
/// without mounting it — the Swift mirror of the web's `renameTitle.ts` and,
/// through both, of the gateway's `validate_session_title`.
///
/// The client normalizes rather than merely validating, because the value it
/// sends is also the value it renders optimistically: the endpoint stores the
/// normalized form and broadcasts THAT back, so a row showing the raw draft
/// would visibly rewrite itself moments later.
enum RenameTitle {
    /// Mirrors `baybo_model::MAX_SESSION_TITLE_LEN`. The server is the authority
    /// and answers 400 past it; bounding here just keeps the user from typing
    /// into a rejection. Counted in Unicode scalars to match Rust's
    /// `chars().count()` rather than Swift's grapheme clusters, so a CJK or
    /// emoji title agrees with the server about where the cap falls.
    static let maxLength = 80

    /// Clip to the cap, counting the way the server counts.
    static func cap(_ text: String) -> String {
        let scalars = text.unicodeScalars
        guard scalars.count > maxLength else { return text }
        return String(String.UnicodeScalarView(scalars.prefix(maxLength)))
    }

    /// What the gateway will store for `text`: interior whitespace collapsed and
    /// the ends trimmed, then capped. A title renders on one line in every
    /// client, so a stored newline would only ever surface as a layout bug on
    /// whichever surface forgot to strip it — which is why the server collapses,
    /// and why this mirrors it instead of merely trimming.
    static func normalized(_ text: String) -> String {
        cap(text.split(whereSeparator: \.isWhitespace).joined(separator: " "))
    }

    /// The draft the editor opens with: whatever the row currently shows.
    ///
    /// Capped, because a title minted server-side (a cron fire's) predates no
    /// such bound — seeding one whole would produce a draft the server refuses
    /// even if the user changes nothing.
    static func seed(title: String?, userText: String?) -> String {
        cap(title ?? userText ?? "")
    }

    /// The title to send, or `nil` to send nothing.
    ///
    /// Compared against the SEED rather than the row's stored title: an
    /// untouched editor must commit nothing, and for an untitled row the seed is
    /// the user's last message — committing that would rename the conversation
    /// to its own preview *and* settle it against the auto-titler, which only
    /// ever writes into a conversation that still has no title.
    static func toCommit(draft: String, seed: String) -> String? {
        let title = normalized(draft)
        guard !title.isEmpty, title != normalized(seed) else { return nil }
        return title
    }
}
