import Foundation

/// Describes command consequences before a move: entering In Progress may start
/// work, while leaving it never stops an existing run.
enum MoveConsequence {
    struct Row {
        let status: IssueStatus
        /// Nil on a row that does nothing worth warning about.
        let note: String?
        /// Pressing this row queues work. Drives the ⚡ mark.
        let startsRun: Bool
        let needsAssignee: Bool
        let isCurrent: Bool
    }

    static func rows(
        for issue: IssueInfo,
        liveRun: IssueRunInfo?,
        assigneeHandle: String?,
        overCeiling: Bool = false,
        heldCeiling: HeldCeiling = .unknown
    ) -> [Row] {
        // A manual move is not rejected by a ceiling; the server may enqueue it
        // as held, so the copy says "may be held" rather than predicting.
        let hasLiveRun = liveRun != nil
        return [IssueStatus.backlog, .todo, .inProgress, .review, .done].map { target in
            Row(
                status: target,
                note: note(
                    target: target,
                    current: issue.status,
                    hasLiveRun: hasLiveRun,
                    assigneeHandle: assigneeHandle,
                    overCeiling: overCeiling,
                    heldCeiling: heldCeiling
                ),
                startsRun: target == .inProgress && issue.status != .inProgress
                    && assigneeHandle != nil,
                needsAssignee: target == .inProgress && assigneeHandle == nil,
                isCurrent: target == issue.status
            )
        }
    }

    /// Which ceiling is doing the stopping. Never say "over its daily budget"
    /// when tokens are what ran out.
    enum HeldCeiling: Equatable {
        case money
        case tokens
        case unknown

        var phrase: String {
            switch self {
            case .money: "the daily budget"
            case .tokens: "the daily token budget"
            case .unknown: "a daily ceiling"
            }
        }
    }

    private static func note(
        target: IssueStatus,
        current: IssueStatus,
        hasLiveRun: Bool,
        assigneeHandle: String?,
        overCeiling: Bool,
        heldCeiling: HeldCeiling
    ) -> String? {
        if target == current { return "current" }
        switch target {
        case .inProgress:
            return startingNote(
                assigneeHandle: assigneeHandle, overCeiling: overCeiling,
                heldCeiling: heldCeiling, arriving: "then it moves")
        case .done:
            return hasLiveRun
                ? "The run keeps going · reclaims the worktree once it settles, then nothing runs again"
                : "Reclaims the worktree · nothing runs again"
        case .backlog, .todo, .review:
            return hasLiveRun ? "The run keeps going — only Stop ends it" : nil
        case .unknown:
            return nil
        }
    }

    static func startingNote(
        assigneeHandle: String?,
        overCeiling: Bool,
        heldCeiling: HeldCeiling,
        arriving: String
    ) -> String {
        guard let assignee = assigneeHandle else {
            return "Needs an assignee first — pick who is on it, \(arriving)"
        }
        if overCeiling {
            return
                "Starts a run for @\(assignee) — may be held, the board is over \(heldCeiling.phrase)"
        }
        return "Starts a run: @\(assignee) reads the card now"
    }

    static func openingNote(
        in status: IssueStatus,
        assigneeHandle: String?,
        overCeiling: Bool = false,
        heldCeiling: HeldCeiling = .unknown
    ) -> String? {
        guard status == .inProgress else { return nil }
        return startingNote(
            assigneeHandle: assigneeHandle, overCeiling: overCeiling,
            heldCeiling: heldCeiling, arriving: "then it opens")
    }

    /// Whether the board will REFUSE a card opened this way. The server's own
    /// rule (`validate_staffing`): In Progress needs somebody on it.
    static func refusesOpening(in status: IssueStatus, assignee: String?) -> Bool {
        status == .inProgress && assignee == nil
    }

    static func toast(afterMoving number: Int64, to row: Row, assigneeHandle: String?) -> String {
        if row.startsRun, let assignee = assigneeHandle {
            return "Queued for @\(assignee) — #\(number)"
        }
        return "Moved #\(number) to \(label(row.status))"
    }

    static func isUndoable(_ row: Row) -> Bool { !row.startsRun }

    static func label(_ status: IssueStatus) -> String {
        switch status {
        case .backlog: "Backlog"
        case .todo: "Todo"
        case .inProgress: "In Progress"
        case .review: "Review"
        case .done: "Done"
        case .unknown: "Unknown"
        }
    }
}
