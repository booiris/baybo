import Foundation

/// What moving a card to a column will do, said on the row before it is
/// tapped.
///
/// A drag is a command, not a view change — entering In Progress with an
/// assignee is the board's single execution trigger — and the phone has no
/// drag to make that obvious. Every row in the Move sheet therefore states
/// its consequence, and the two the operator most needs are the ones a
/// desktop board never has to say out loud: that a move **out** of In
/// Progress does not stop the run, and that a move **in** starts one.
enum MoveConsequence {
    struct Row {
        let status: IssueStatus
        /// Nil on a row that does nothing worth warning about.
        let note: String?
        /// Pressing this row queues work. Drives the ⚡ mark.
        let startsRun: Bool
        /// Pressing this opens the assignee picker first, then moves. Never a
        /// dead row: the server refuses In Progress without an assignee, and
        /// a disabled row leaves the operator to guess why.
        let needsAssignee: Bool
        let isCurrent: Bool
    }

    /// Every column, in board order, with what each would do to this card.
    ///
    /// `overCeiling` only softens the wording — a manual move is **never**
    /// gated on the run ceiling, which gates the board's own promotions
    /// alone. The server decides at enqueue whether the run is held, so the
    /// row says "may be held" rather than claiming to know.
    static func rows(
        for issue: IssueInfo,
        liveRun: IssueRunInfo?,
        assigneeHandle: String?,
        overCeiling: Bool = false,
        heldCeiling: HeldCeiling = .unknown
    ) -> [Row] {
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
            guard let assignee = assigneeHandle else {
                return "Needs an assignee first — pick who is on it, then it moves"
            }
            if overCeiling {
                return
                    "Starts a run for @\(assignee) — may be held, the board is over \(heldCeiling.phrase)"
            }
            return "Starts a run: @\(assignee) reads the card now"
        case .done:
            // Entering Done reclaims the checkout — but only once whatever is
            // running has settled, and a dirty worktree is kept with a note
            // rather than thrown away.
            return hasLiveRun
                ? "The run keeps going · reclaims the worktree once it settles, then nothing runs again"
                : "Reclaims the worktree · nothing runs again"
        case .backlog, .todo, .review:
            // THE sentence this whole sheet exists for. Moving a card out of
            // In Progress does not stop its run — the shimmer follows the run,
            // not the column — and Stop on the card is the only kill switch
            // there is.
            return hasLiveRun ? "The run keeps going — only Stop ends it" : nil
        case .unknown:
            return nil
        }
    }

    /// What the toast says after a move landed.
    ///
    /// **Only a move that actually queued work may say so.** An assign or a
    /// handover moves an existing run into another session rather than
    /// enqueuing, and a same-card duplicate is refused outright and shows up
    /// only in the timeline — so a client that announced "Queued" for either
    /// would be asserting something the server never said.
    static func toast(afterMoving number: Int64, to row: Row, assigneeHandle: String?) -> String {
        if row.startsRun, let assignee = assigneeHandle {
            return "Queued for @\(assignee) — #\(number)"
        }
        return "Moved #\(number) to \(label(row.status))"
    }

    /// Whether the move can be undone from its toast.
    ///
    /// A move that started a run cannot: undoing it would put the card back
    /// while the run it triggered keeps going, so the toast would be offering
    /// to unwind something it cannot reach.
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
