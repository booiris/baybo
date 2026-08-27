import Foundation

enum BoardOrder {
    /// Render-only bands. Their pinned/unread/recency order is never sent back
    /// as the board's persisted position order.
    struct Bands {
        var pinned: [IssueInfo] = []
        var new: [IssueInfo] = []
        var queue: [IssueInfo] = []

        var all: [IssueInfo] { pinned + new + queue }
        var showsHeaders: Bool {
            [pinned, new, queue].filter { !$0.isEmpty }.count > 1
        }
    }

    static func hasNews(_ issue: IssueInfo) -> Bool {
        // Cancelled cards remain filterable history and are never lifted as new.
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

    private static func newestFirst(_ issues: [IssueInfo]) -> [IssueInfo] {
        issues.sorted {
            if $0.updatedAtMs != $1.updatedAtMs { return $0.updatedAtMs > $1.updatedAtMs }
            if $0.position != $1.position { return $0.position < $1.position }
            return $0.number < $1.number
        }
    }

    static func liveCount(_ issues: [IssueInfo]) -> Int {
        issues.count { $0.cancelledAtMs == nil }
    }

    static func hasNews(inStage issues: [IssueInfo]) -> Bool {
        issues.contains(where: hasNews)
    }
}
