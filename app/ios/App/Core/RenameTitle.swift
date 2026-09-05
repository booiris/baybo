import Foundation

/// Conversation-rename rules — a forwarder to the shared core, kept as a type
/// so the call sites read the same as they always did.
///
/// The rules used to live here, in `app/web`'s `renameTitle.ts`, and in the
/// gateway's `validate_session_title`, which meant three implementations of one
/// question and, once Android arrived, four. The whitespace collapse and the
/// scalar cap now live once in `baybo_model`; the client policy over them lives
/// once in the core (`stores/title.rs`), which is what both phone shells call.
/// `renameTitle.ts` stays a mirror because nothing can be shared across that
/// boundary — its port-fidelity test is what notices when it drifts.
///
/// The client normalizes rather than merely validating, because the value it
/// sends is also the value it renders optimistically: the endpoint stores the
/// normalized form and broadcasts THAT back, so a row showing the raw draft
/// would visibly rewrite itself moments later.
enum RenameTitle {
    /// Mirrors `baybo_model::MAX_SESSION_TITLE_LEN`. The server is the authority
    /// and answers 400 past it; bounding here just keeps the user from typing
    /// into a rejection. Counted in Unicode scalars to match Rust's `chars()`
    /// rather than Swift's grapheme clusters, so a CJK or emoji title agrees
    /// with the server about where the cap falls.
    static let maxLength = 80

    /// Clip to the cap, counting the way the server counts.
    static func cap(_ text: String) -> String {
        sessionTitleCap(text: text)
    }

    /// What the gateway will store for `text`: interior whitespace collapsed
    /// and the ends trimmed, then capped.
    static func normalized(_ text: String) -> String {
        sessionTitleNormalized(text: text)
    }

    /// The draft the editor opens with: whatever the row currently shows.
    static func seed(title: String?, userText: String?) -> String {
        sessionTitleSeed(title: title, userText: userText)
    }

    /// The title to send, or `nil` to send nothing.
    static func toCommit(draft: String, seed: String) -> String? {
        sessionTitleToCommit(draft: draft, seed: seed)
    }
}
