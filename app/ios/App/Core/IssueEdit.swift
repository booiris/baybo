import Foundation

extension IssueInfo {
    /// A copy of this card with a field or two changed.
    ///
    /// UniFFI generates records with `let` properties, so an optimistic edit
    /// cannot move a field in place — and spelling out the full initialiser at
    /// each call site is how a field added upstream ends up silently dropped
    /// by whichever site nobody remembered to update. Here it is written once,
    /// and a new field on the record fails THIS file to compile, which is the
    /// failure worth having.
    ///
    /// `assignee` takes a `StringPatch` rather than an `Optional` for the same
    /// reason the wire does: `nil` has to be able to mean "leave it" and
    /// "clear it", and one optional cannot say both.
    func with(
        status: IssueStatus? = nil,
        priority: IssuePriority? = nil,
        pinned: Bool? = nil,
        assignee: StringPatch = .keep,
        description: String? = nil,
        branch: StringPatch = .keep,
        blockedReason: StringPatch = .keep,
        unread: Int64? = nil,
        lastRunFailed: Bool? = nil,
        cancelled: Bool? = nil
    ) -> IssueInfo {
        let nextAssignee: String? =
            switch assignee {
            case .keep: self.assignee
            case .clear: nil
            case let .set(value): value
            }
        let nextBranch: String? =
            switch branch {
            case .keep: self.branch
            case .clear: nil
            case let .set(value): value
            }
        let nextBlocked: String? =
            switch blockedReason {
            case .keep: self.blockedReason
            case .clear: nil
            case let .set(value): value
            }
        let nextCancelledAtMs: Int64? =
            switch cancelled {
            case nil: cancelledAtMs
            case true: Int64(Date().timeIntervalSince1970 * 1000)
            case false: nil
            }
        return IssueInfo(
            number: number,
            projectId: projectId,
            title: title,
            description: description ?? self.description,
            attachments: attachments,
            status: status ?? self.status,
            priority: priority ?? self.priority,
            assignee: nextAssignee,
            position: position,
            pinned: pinned ?? self.pinned,
            branch: nextBranch,
            blockedReason: nextBlocked,
            parent: parent,
            filedFrom: filedFrom,
            stage: stage,
            subIssues: subIssues,
            unread: unread ?? self.unread,
            lastRunFailed: lastRunFailed ?? self.lastRunFailed,
            approvalPending: approvalPending,
            openedByAgent: openedByAgent,
            cancelledAtMs: nextCancelledAtMs,
            createdAtMs: createdAtMs,
            updatedAtMs: updatedAtMs)
    }
}
