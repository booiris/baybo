import Foundation

/// Excerpt + highlight for a search result card.
///
/// A port of `app/web/src/pages/chat/searchSnippet.ts`, held to it byte for byte
/// by the shared vectors in `app/web/src/pages/chat/searchSnippetVectors.json`
/// (`SearchSnippetVectorTests`). The rules live in that file's comments; what
/// follows is only how they land in Swift. Two ports and one contract is the
/// arrangement everywhere the transcript already lives — see
/// `app/mobile/web/src/restSentinel.ts` — so the discipline is the fixture, not a
/// second source of truth.
///
/// Everything here works in GRAPHEME CLUSTER space, which Swift gives for free:
/// `Array(text)` is `[Character]`, and a `Character` is a cluster. The JS side
/// had to reach for `Intl.Segmenter` to meet it there. That is what keeps a
/// window edge from splitting an emoji ZWJ sequence or stranding a combining
/// mark — and, on the JS side, from emitting a lone surrogate.
enum SearchSnippet {
    /// Clusters of prose kept BEFORE the match — front-loaded so the highlight
    /// survives the result card's line clamp; the full why lives on the JS
    /// constants. Must equal the JS `LEAD_PAD` / `TRAIL_PAD`; the vectors fail
    /// loudly if they drift.
    private static let leadPad = 12
    /// And AFTER it — the rest of the same budget.
    private static let trailPad = 108
    /// Head shown when no term can be located.
    private static let headLength = leadPad + trailPad

    /// A run of the message, flagged when it is one of the query's terms.
    struct Segment: Equatable {
        let text: String
        let match: Bool
    }

    /// Split a query the way the server does: the user's own whitespace is the
    /// AND boundary, and a chunk with nothing alphanumeric contributes no tokens
    /// and is dropped rather than searched for.
    ///
    /// `isLetter` / `isNumber` are the `\p{L}` / `\p{N}` the JS regex uses —
    /// note `isNumber`, not `isWholeNumber`, since `\p{N}` covers Nl and No too.
    static func queryChunks(_ query: String) -> [String] {
        query
            .split(whereSeparator: \.isWhitespace)
            .map(String.init)
            .filter { $0.contains { $0.isLetter || $0.isNumber } }
    }

    /// Case-fold cluster by cluster, never the string as a whole.
    ///
    /// Folding a whole string can change its LENGTH (`İ` lowercases to two
    /// scalars), which slides every subsequent index — so matching on a folded
    /// string and slicing the original is only correct until someone types
    /// Turkish. Per-cluster folding keeps the folded array index-aligned with the
    /// original by construction, whatever any one cluster does.
    private static func folded(_ clusters: [Character]) -> [String] {
        clusters.map { $0.lowercased() }
    }

    /// Cluster indices where `needle` occurs in `haystack` (both already folded).
    private static func occurrences(_ haystack: [String], _ needle: [String]) -> [Int] {
        guard !needle.isEmpty, needle.count <= haystack.count else { return [] }
        var out: [Int] = []
        var at = 0
        while at + needle.count <= haystack.count {
            if Array(haystack[at..<(at + needle.count)]) == needle {
                out.append(at)
                at += needle.count
            } else {
                at += 1
            }
        }
        return out
    }

    /// Cut a window of `text` around what the query matched, flagging the terms.
    ///
    /// Anchors on the EARLIEST term rather than the whole input: the server ANDs
    /// each chunk independently, so `foo bar` legitimately matches a message
    /// reading `bar … foo`. Searching for the literal input finds nothing there
    /// and falls back to the head — a real hit rendered with an excerpt holding
    /// none of what was searched for.
    static func snippet(_ text: String, query: String) -> [Segment] {
        let g = Array(text)
        let gl = folded(g)

        var hits: [(at: Int, end: Int)] = []
        for chunk in queryChunks(query) {
            let needle = folded(Array(chunk))
            for at in occurrences(gl, needle) {
                hits.append((at: at, end: at + needle.count))
            }
        }

        func cut(_ from: Int, _ to: Int) -> String {
            guard from < to else { return "" }
            return String(g[from..<min(to, g.count)])
        }

        if hits.isEmpty {
            let head = cut(0, min(headLength, g.count))
            return [Segment(text: head + (g.count > headLength ? "…" : ""), match: false)]
        }

        // Stable sort: two terms matching at the same index must keep the order
        // the query listed them, exactly as JS's `Array.sort` comparator leaves
        // them. Swift's `sort` is NOT guaranteed stable, so sort on the pair.
        hits = hits.enumerated()
            .sorted { ($0.element.at, $0.offset) < ($1.element.at, $1.offset) }
            .map(\.element)

        let anchor = hits[0]
        let from = max(0, anchor.at - leadPad)
        let to = min(g.count, anchor.end + trailPad)

        var segments: [Segment] = []
        func push(_ t: String, _ match: Bool) {
            if !t.isEmpty { segments.append(Segment(text: t, match: match)) }
        }

        var cursor = from
        for hit in hits {
            // Highlight every term landing in the window, not just the anchor.
            // Overlapping hits (a term inside another) collapse into the first.
            if hit.at < cursor || hit.end > to { continue }
            push(cut(cursor, hit.at), false)
            push(cut(hit.at, hit.end), true)
            cursor = hit.end
        }
        push(cut(cursor, to), false)

        if from > 0 { segments.insert(Segment(text: "…", match: false), at: 0) }
        if to < g.count { segments.append(Segment(text: "…", match: false)) }
        return segments
    }
}
