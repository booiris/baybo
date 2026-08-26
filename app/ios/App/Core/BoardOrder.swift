import Foundation

/// The order a stage's cards are READ in, and the bands they are drawn under.
///
/// Two lifts and recency sit above the board's own `position`, and all are
/// rendered orders only — nothing here ever writes a position back. Inside
/// each rank, `updatedAtMs` descends; position and number make equal timestamps
/// deterministic and preserve the stored order as the fallback.
///
/// The ranking is **pinned, then unread, then newest updated**, and the
/// reason that order and not the reverse: a pin is what somebody chose, an
/// unread count is what happened to a card while they were elsewhere, and
/// what was chosen outranks what arrived. The unread lift applies again
/// *inside* the pinned block, so a pinned card carrying a comment leads it.
///
/// Mirrors `app/web`'s `boardModel.readingOrder`. The two disagree about
/// nothing, and `boardModel.test.ts` pins the invariant that matters most:
/// concatenating the bands returns the column unchanged, so banding is a
/// grouping and never a second sort.
enum BoardOrder {
    /// Cards grouped for display. A header is only worth drawing when more
    /// than one band is non-empty — one header over a whole wall separates
    /// nothing.
    struct Bands {
        var pinned: [IssueInfo] = []
        var new: [IssueInfo] = []
        var queue: [IssueInfo] = []

        var all: [IssueInfo] { pinned + new + queue }
        var showsHeaders: Bool {
            [pinned, new, queue].filter { !$0.isEmpty }.count > 1
        }
    }

    /// Something an agent did on this card while the operator was elsewhere.
    ///
    /// A **cancelled card is never lifted by it**: cancel is terminal, and
    /// floating a struck-through card over live work because somebody spoke
    /// on it before it was called off is the board arguing with itself. A pin
    /// still lifts one — nothing but the operator put that pin there, and a
    /// control that goes on offering itself while quietly refusing to work is
    /// worse than a struck-through card at the top.
    static func hasNews(_ issue: IssueInfo) -> Bool {
        issue.unread > 0 && issue.cancelledAtMs == nil
    }

    static func bands(_ issues: [IssueInfo]) -> Bands {
        var bands = Bands()
        for issue in newestFirst(issues) {
            if issue.pinned {
                bands.pinned.append(issue)
            } else if hasNews(issue) {
                bands.new.append(issue)
            } else {
                bands.queue.append(issue)
            }
        }
        // The unread lift applies inside the pinned block too.
        bands.pinned = bands.pinned.filter(hasNews) + bands.pinned.filter { !hasNews($0) }
        return bands
    }

    static func reading(_ issues: [IssueInfo]) -> [IssueInfo] {
        bands(issues).all
    }

    /// Recency made total by the board's stored order. `position` is a dense
    /// rank the server renumbers per move, but two cards can still share one
    /// across a refetch mid-move, so `number` is the final visible tie-breaker.
    private static func newestFirst(_ issues: [IssueInfo]) -> [IssueInfo] {
        issues.sorted {
            if $0.updatedAtMs != $1.updatedAtMs { return $0.updatedAtMs > $1.updatedAtMs }
            if $0.position != $1.position { return $0.position < $1.position }
            return $0.number < $1.number
        }
    }

    /// What a stage's segment counts. **Cancelled cards are excluded** — the
    /// number measures live work, and the server's own column counts do the
    /// same.
    static func liveCount(_ issues: [IssueInfo]) -> Int {
        issues.count { $0.cancelledAtMs == nil }
    }

    /// Whether a stage the operator is not looking at has something new, which
    /// is what its red dot means. A dot rather than a number, because pressing
    /// the segment cannot discharge it — opening the cards does.
    static func hasNews(inStage issues: [IssueInfo]) -> Bool {
        issues.contains(where: hasNews)
    }
}
