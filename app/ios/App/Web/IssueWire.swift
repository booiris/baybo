import Foundation

enum IssueWire {
    static func card(_ issue: IssueInfo) -> [String: Any] {
        var out: [String: Any] = [
            "number": issue.number,
            "project_id": issue.projectId,
            "title": issue.title,
            "description": issue.description,
            "status": word(issue.status),
            "priority": word(issue.priority),
            "position": issue.position,
            "pinned": issue.pinned,
            "stage": issue.stage,
            "unread": issue.unread,
            "last_run_failed": issue.lastRunFailed,
            "approval_pending": issue.approvalPending,
            "opened_by_agent": issue.openedByAgent,
            "created_at_ms": issue.createdAtMs,
            "updated_at_ms": issue.updatedAtMs,
        ]
        if !issue.attachments.isEmpty {
            out["attachments"] = issue.attachments.map(attachment(_:))
        }
        if let assignee = issue.assignee { out["assignee"] = assignee }
        if let branch = issue.branch { out["branch"] = branch }
        if let blocked = issue.blockedReason { out["blocked_reason"] = blocked }
        if let parent = issue.parent { out["parent"] = parent }
        if let filedFrom = issue.filedFrom { out["filed_from"] = filedFrom }
        if let subIssues = issue.subIssues {
            out["sub_issues"] = ["done": subIssues.done, "total": subIssues.total]
        }
        if let cancelled = issue.cancelledAtMs { out["cancelled_at_ms"] = cancelled }
        return out
    }

    static func run(_ run: IssueRunInfo) -> [String: Any] {
        var out: [String: Any] = [
            "number": run.number,
            "attempt": run.attempt,
            "agent_id": run.agentId,
            "status": word(run.status),
            "trigger": word(run.trigger),
            "created_at_ms": run.createdAtMs,
        ]
        if let sessionId = run.sessionId { out["session_id"] = sessionId }
        if let error = run.error { out["error"] = error }
        if let started = run.startedAtMs { out["started_at_ms"] = started }
        if let settled = run.settledAtMs { out["settled_at_ms"] = settled }
        if let cost = run.costMicros { out["cost_micros"] = cost }
        if let input = run.inputTokens { out["input_tokens"] = input }
        if let output = run.outputTokens { out["output_tokens"] = output }
        return out
    }

    static func child(_ issue: IssueInfo) -> [String: Any] {
        var out: [String: Any] = [
            "number": issue.number,
            "title": issue.title,
            "status": word(issue.status),
        ]
        if let cancelled = issue.cancelledAtMs { out["cancelled_at_ms"] = cancelled }
        return out
    }

    static func person(_ person: IssuePerson) -> [String: Any] {
        var out: [String: Any] = [
            "handle": person.handle,
            "monogram": person.monogram,
        ]
        if let avatar = person.avatar { out["avatar"] = avatar }
        return out
    }

    static func attachment(_ a: IssueAttachmentInfo) -> [String: Any] {
        var out: [String: Any] = [
            "blob_id": a.blobId,
            "mime_type": a.mimeType,
            "size": a.size,
        ]
        if let filename = a.filename { out["filename"] = filename }
        return out
    }

    // MARK: - Enum words
    // Never encode `unknown`; use the nearest value understood by the page.

    static func word(_ status: IssueStatus) -> String {
        switch status {
        case .backlog: "backlog"
        case .todo: "todo"
        case .inProgress: "in_progress"
        case .review: "review"
        case .done: "done"
        // A status this build could not read is not a status it may invent one
        // for; Backlog is where an unplaceable card belongs.
        case .unknown: "backlog"
        }
    }

    static func word(_ priority: IssuePriority) -> String {
        switch priority {
        case .urgent: "urgent"
        case .high: "high"
        case .medium: "medium"
        case .low: "low"
        case .none, .unknown: "none"
        }
    }

    static func word(_ status: RunStatus) -> String {
        switch status {
        case .queued: "queued"
        case .held: "held"
        case .running: "running"
        case .done: "done"
        case .failed: "failed"
        case .cancelled, .unknown: "cancelled"
        }
    }

    /// The page prints a trigger and never matches on one, so its mirror keeps
    /// the type wide — which is why an unknown one may pass through as itself.
    static func word(_ trigger: RunTrigger) -> String {
        switch trigger {
        case .started: "started"
        case .assigned: "assigned"
        case .retry: "retry"
        case .comment: "comment"
        case .promoted: "promoted"
        case .triage: "triage"
        case .stageBarrier: "stage_barrier"
        case .review: "review"
        case .stalled: "stalled"
        case .blocked: "blocked"
        case .grooming: "grooming"
        case .boardIdle: "board_idle"
        case .unknown: "unknown"
        }
    }
}
