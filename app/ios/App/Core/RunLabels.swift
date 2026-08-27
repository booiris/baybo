import Foundation

/// Card run state follows the unsettled row; the assignee and actual runner may
/// differ for coordination runs and must not be conflated.
enum RunLabels {
    static func liveRun(for number: Int64, in runs: [IssueRunInfo]) -> IssueRunInfo? {
        runs.first { $0.number == number && $0.settledAtMs == nil }
    }

    static func word(for run: IssueRunInfo?) -> String? {
        // Queued can start when a slot frees; held waits for a ceiling change.
        switch run?.status {
        case .running: "WORKING"
        case .queued: "QUEUED"
        case .held: "HELD"
        // A requeued row reads `queued` and carries a session; there is no
        // interrupted state on a card's face, deliberately.
        case .done, .failed, .cancelled, .unknown, .none: nil
        }
    }

    static func elapsed(for run: IssueRunInfo?, now: Date = Date()) -> String? {
        guard let run, run.settledAtMs == nil else { return nil }
        let sinceMs: Int64? =
            switch run.status {
            case .running: run.startedAtMs ?? run.createdAtMs
            case .queued, .held: run.createdAtMs
            case .done, .failed, .cancelled, .unknown: nil
            }
        guard let sinceMs else { return nil }
        let seconds = Int(now.timeIntervalSince1970 - Double(sinceMs) / 1000)
        return seconds < 0 ? nil : compact(seconds: seconds)
    }

    /// `4m`, `2h`, `41s` — the card has room for one unit, not a duration.
    static func compact(seconds: Int) -> String {
        if seconds < 60 { return "\(seconds)s" }
        if seconds < 3600 { return "\(seconds / 60)m" }
        if seconds < 86400 { return "\(seconds / 3600)h" }
        return "\(seconds / 86400)d"
    }

    /// A settled run's duration, for the execution log: `2m10s`.
    static func duration(of run: IssueRunInfo) -> String? {
        guard let settled = run.settledAtMs, let started = run.startedAtMs else { return nil }
        let seconds = max(0, Int((settled - started) / 1000))
        if seconds < 60 { return "\(seconds)s" }
        let minutes = seconds / 60
        let rest = seconds % 60
        if minutes < 60 { return rest == 0 ? "\(minutes)m" : "\(minutes)m\(rest)s" }
        return "\(minutes / 60)h\(minutes % 60)m"
    }

    static func runnerDiffersFromAssignee(run: IssueRunInfo?, assignee: String?) -> Bool {
        guard let run, run.settledAtMs == nil else { return false }
        return run.agentId != assignee
    }

    /// What the execution log calls the reason a run started. Verbatim from
    /// `app/web`'s own labels, so the two logs read the same.
    static func triggerLabel(_ trigger: RunTrigger) -> String {
        switch trigger {
        case .started: "moved to In Progress"
        case .assigned: "assigned"
        case .retry: "retry"
        case .comment: "comment"
        case .promoted: "the board had room"
        case .triage: "nobody assigned"
        case .stageBarrier: "stage barrier"
        case .review: "awaiting review"
        case .stalled: "work stopped"
        case .blocked: "blocked, needs a decision"
        case .grooming: "grooming"
        case .boardIdle: "the board was idle"
        case .unknown: "started"
        }
    }
}
