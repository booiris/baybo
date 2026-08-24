import Foundation

/// The order a stage's cards are READ in, and the bands they are drawn under.
///
/// Two lifts sit above the board's own `position`, and both are rendered
/// orders only — nothing here ever writes a position back. A drag on the desk
/// still sends the stored order, and a card settles back across a band
/// boundary on the next refetch, which is the cost both lifts already charge
/// on the web.
///
/// The ranking is **pinned, then unread, then the board's own**, and the
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
        for issue in stableByPosition(issues) {
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

    /// The board's own order, made total so the two lifts are the only thing
    /// that moves a card. `position` is a dense rank the server renumbers per
    /// move, but two cards can still share one across a refetch mid-move —
    /// `number` breaks that tie the same way the server's own pick order
    /// does, rather than leaving it to sort stability nobody can see.
    private static func stableByPosition(_ issues: [IssueInfo]) -> [IssueInfo] {
        issues.sorted {
            $0.position == $1.position ? $0.number < $1.number : $0.position < $1.position
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
