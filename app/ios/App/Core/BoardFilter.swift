import Foundation

struct BoardFilter: Equatable {
    var assignee: String?
    var priority: IssuePriority?
    var runningOnly = false
    var showsCancelled = false

    var count: Int {
        [assignee != nil, priority != nil, runningOnly].count { $0 }
    }

    var isActive: Bool { count > 0 }

    func apply(_ issues: [IssueInfo], runs: [IssueRunInfo]) -> [IssueInfo] {
        // Cancellation is a view filter, never deletion from the board model.
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
