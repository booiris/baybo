import Foundation

/// What the board is currently showing less of.
///
/// A pure value with a pure `apply`, so what a narrowed board contains is
/// testable without a screen — and so the filter chip's count and the rows it
/// hides can never disagree, because both read the same struct.
///
/// **Cancelled is a filter, not a rule.** A cancelled card is hidden by
/// default because a board is a place of live work, but it is still a row on
/// the board and still openable: the alternative — dropping it from the client
/// entirely — is how a card somebody wants to reopen becomes unreachable from
/// the phone.
struct BoardFilter: Equatable {
    var assignee: String?
    var priority: IssuePriority?
    var runningOnly = false
    var showsCancelled = false

    /// How many narrowings are in force. Drives the chip's count; `showsCancelled`
    /// is not one of them — it WIDENS the board, and counting it would have an
    /// un-narrowed board wearing a filter mark.
    var count: Int {
        [assignee != nil, priority != nil, runningOnly].count { $0 }
    }

    var isActive: Bool { count > 0 }

    func apply(_ issues: [IssueInfo], runs: [IssueRunInfo]) -> [IssueInfo] {
        issues.filter { issue in
            if !showsCancelled, issue.cancelledAtMs != nil { return false }
            if let assignee, issue.assignee != assignee { return false }
            if let priority, issue.priority != priority { return false }
            if runningOnly, RunLabels.liveRun(for: issue.number, in: runs) == nil { return false }
            return true
        }
    }

    mutating func clear() {
        assignee = nil
        priority = nil
        runningOnly = false
    }
}
