import Foundation

/// The two-or-three letters inside an agent's face.
///
/// A property of the SET, not of one handle, which is the whole point: the
/// obvious rule — first letter of each dash-segment — collides on real handles
/// (`dev-1` and `docs-1` both give `D1`), and two identical faces standing for
/// different agents is worse than a longer monogram. So this lives here rather
/// than inside a view: every list that draws a team — the board's face row, the
/// assignee picker, the filter sheet — must reach the same answer, and the
/// first version of this rule lived inside `TeamFaces` and let the picker and
/// the filter go on printing `D1` twice.
enum AgentMonogram {
    /// How many letters of the FIRST segment may be used. A dashed handle
    /// appends its second segment's first character, so this is one less than
    /// the glyph ceiling — three glyphs is what a 22pt circle can carry, and
    /// getting this wrong is how `reviewer-1` came out as the four-glyph
    /// `REV1`. Past the cap, duplicates are simply what the row shows:
    /// unreadable is not an improvement on ambiguous.
    private static let maxLeading = 2

    /// Monograms for a team, keyed by agent id, made unique across it.
    ///
    /// When one pair collides the WHOLE set widens, not just the pair: a row
    /// reading `DE1 D2 DO1` makes the odd one out look like a different kind
    /// of thing, when all it means is that its neighbours happened to clash.
    static func map(for members: [TeamMemberInfo]) -> [String: String] {
        var out: [String: String] = [:]
        for width in 1...maxLeading {
            out = Dictionary(
                uniqueKeysWithValues: members.map { ($0.id, of($0.handle, leading: width)) })
            if Set(out.values).count == members.count { break }
        }
        return out
    }

    /// One handle's monogram at a given first-segment width. The fallback for
    /// a face drawn with no set around it.
    static func of(_ handle: String, leading: Int = 1) -> String {
        let parts = handle.split(separator: "-")
        guard let first = parts.first else { return String(handle.prefix(2)).uppercased() }
        guard parts.count >= 2, let tail = parts[1].first else {
            return String(first.prefix(max(2, leading))).uppercased()
        }
        return first.prefix(leading).uppercased() + String(tail).uppercased()
    }
}
