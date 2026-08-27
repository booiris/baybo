import Foundation

/// A half-typed `@handle` in the card's comment field: where it starts, and
/// what has been typed of it.
struct IssueMentionQuery: Equatable {
    /// UTF-16 offset of the `@` in the draft.
    let start: Int
    /// What follows it, possibly empty — a bare `@` asks for the whole roster.
    let prefix: String
}

struct IssueMentionCompletion: Equatable {
    /// The UTF-16 range of the draft it replaces — the `@`, what was typed
    /// after it, and a space already sitting behind it.
    let range: Range<Int>
    /// What goes in that range.
    let text: String
    /// The whole draft, completed.
    let draft: String
}

enum IssueMention {
    // Mentions only complete roster handles; they never predict staffing.
    private static let at: UInt16 = 0x40
    private static let leftParen: UInt16 = 0x28
    private static let space: UInt16 = 0x20
    private static let hyphen: UInt16 = 0x2D

    /// The mention being typed at `caret`, if there is one.
    static func query(in text: String, caret: Int) -> IssueMentionQuery? {
        let units = Array(text.utf16)
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
