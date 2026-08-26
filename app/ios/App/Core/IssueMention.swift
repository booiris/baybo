import Foundation

/// A half-typed `@handle` in the card's comment field: where it starts, and
/// what has been typed of it.
struct IssueMentionQuery: Equatable {
    /// UTF-16 offset of the `@` in the draft.
    let start: Int
    /// What follows it, possibly empty — a bare `@` asks for the whole roster.
    let prefix: String
}

/// Completing a query, in both of the forms the dock needs it: the EDIT (the
/// live UIKit document takes it, so a caret in the middle of a draft stays
/// where the operator put it) and the finished DRAFT (the binding takes that).
///
/// **Both come off one snapshot of the text, in one call, deliberately.** The
/// dock used to ask for the edit, apply it to the document, and then ask for
/// the draft — and the document write updates the binding under it, so the
/// second question was answered about text that already carried the answer.
/// The edit went in twice and the handle came out doubled (`@dev-1 ev-1 `).
/// There is no second question to get wrong now.
struct IssueMentionCompletion: Equatable {
    /// The UTF-16 range of the draft it replaces — the `@`, what was typed
    /// after it, and a space already sitting behind it.
    let range: Range<Int>
    /// What goes in that range.
    let text: String
    /// The whole draft, completed.
    let draft: String
}

/// Completing a mention in a comment.
///
/// **The grammar is the gateway's** (`crates/project/src/mentions.rs`): a
/// handle is `[a-z0-9-]`, and an `@` only opens one at the start of the text
/// or after whitespace or `(`. A completion offered where the server will not
/// read one is worse than no completion at all — it promises a delivery that
/// silently does not happen — so this half is deliberately no more permissive
/// than the parse half. It is also no LESS permissive by accident: the two are
/// different questions ("is this a mention" vs "am I typing one"), which is
/// why `app/web`'s own composer carries the same grammar a third time.
///
/// **Scanned over UTF-16 code units**, not Characters, because the offsets
/// cross into UIKit: `UITextInput` counts positions in UTF-16 and the
/// completion is applied through it. A grapheme-indexed model would convert at
/// every boundary, and an emoji anywhere earlier in the draft would shift the
/// two counts apart — the mention would then be inserted a few characters off.
/// The grammar itself is pure ASCII, so the scan loses nothing.
enum IssueMention {
    private static let at: UInt16 = 0x40
    private static let leftParen: UInt16 = 0x28
    private static let space: UInt16 = 0x20
    private static let hyphen: UInt16 = 0x2D

    /// The mention being typed at `caret`, if there is one.
    static func query(in text: String, caret: Int) -> IssueMentionQuery? {
        let units = Array(text.utf16)
        // A caret past the end is the SwiftUI binding a frame behind the
        // document it mirrors; clamping keeps the strip up rather than
        // blinking it out for one keystroke.
        let end = min(max(caret, 0), units.count)

        var start = end - 1
        while start >= 0, units[start] != at {
            guard isHandleUnit(units[start]) else { return nil }
            start -= 1
        }
        guard start >= 0 else { return nil }
        if start > 0 {
            let preceding = units[start - 1]
            guard isSpaceUnit(preceding) || preceding == leftParen else { return nil }
        }
        return IssueMentionQuery(
            start: start, prefix: String(decoding: units[(start + 1)..<end], as: UTF16.self))
    }

    /// Who the query could mean, in the order the strip should offer them.
    ///
    /// **The card's assignee leads.** The web's popup keeps roster order
    /// because it shows the whole list at once; a phone shows three chips and
    /// scrolls for the rest, so the order is what most operators will ever
    /// see — and the agent already on the card is who a comment is most often
    /// addressed to.
    static func candidates(in team: [TeamMemberInfo], prefix: String, assignee: String?)
        -> [TeamMemberInfo]
    {
        // No case folding: the handle grammar admits lowercase only, on the
        // server and in `query`'s scan, so there is nothing to fold.
        var matching = team.filter { $0.handle.hasPrefix(prefix) }
        if let assignee, let index = matching.firstIndex(where: { $0.id == assignee }) {
            matching.insert(matching.remove(at: index), at: 0)
        }
        return matching
    }

    /// Turn `query` into `@handle `.
    ///
    /// The trailing space is part of the replacement rather than a decision
    /// about the tail: a mention runs up against the next word otherwise, and
    /// a space already there is swallowed by the range so completing cannot
    /// leave two.
    static func completion(for query: IssueMentionQuery, handle: String, in text: String)
        -> IssueMentionCompletion
    {
        let units = Array(text.utf16)
        let typedEnd = min(query.start + 1 + query.prefix.utf16.count, units.count)
        let end = typedEnd < units.count && units[typedEnd] == space ? typedEnd + 1 : typedEnd
        let written = "@\(handle) "
        let head = String(decoding: units[0..<min(query.start, units.count)], as: UTF16.self)
        let tail = String(decoding: units[end...], as: UTF16.self)
        return IssueMentionCompletion(
            range: query.start..<end, text: written, draft: head + written + tail)
    }

    private static func isHandleUnit(_ unit: UInt16) -> Bool {
        (unit >= 0x61 && unit <= 0x7A) || (unit >= 0x30 && unit <= 0x39) || unit == hyphen
    }

    private static func isSpaceUnit(_ unit: UInt16) -> Bool {
        unit == space || unit == 0x09 || unit == 0x0A || unit == 0x0D
    }
}
