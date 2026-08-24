import Foundation

/// The card page's payload, encoded from the FFI records.
///
/// **This is a mirror, and mirrors drift.** The FFI's `IssueInfo` is already
/// the gateway's `IssueDto` decoded once; this turns it back into the gateway's
/// own JSON shape so `src/issue/types.ts` — which `issueSentinel.ts` pins to
/// the utoipa schema — can read it unchanged. The timeline does not go through
/// here at all: it crosses as the gateway's raw JSON, because its only consumer
/// is the page and a Swift re-encoding would be a third place every new event
/// kind has to be taught about.
///
/// What holds this half honest is `issueCardFixture.json`: the Swift test
/// writes it from a fully-populated record and the vitest test reads it back as
/// an `IssueDetail`. A field renamed on either side fails one of the two.
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
        // Absent, never null: every optional here carries
        // `skip_serializing_if = "Option::is_none"` on the gateway, and the
        // page's mirror is asserted against that under `Undefinedify`. A `null`
        // would type-check nowhere and read as present everywhere.
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

    /// A child card, as the page's `ChildIssue`. Only what a row needs — the
    /// list is drawn from the board's own issues, and sending each one whole
    /// would put a board's worth of cards through the bridge for four lines of
    /// text.
    static func child(_ issue: IssueInfo) -> [String: Any] {
        var out: [String: Any] = [
            "number": issue.number,
            "title": issue.title,
            "status": word(issue.status),
        ]
        if let cancelled = issue.cancelledAtMs { out["cancelled_at_ms"] = cancelled }
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
    //
    // The gateway's own spellings. `unknown` is what the FFI decodes an
    // unrecognised word into, and it must NOT be encoded — sending it would
    // hand the page a value its union has never heard of. It rides back as the
    // nearest honest thing instead.

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
