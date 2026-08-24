import Foundation

/// What a card says about the run on it, and who is running it.
///
/// Mirrors `app/web`'s `boardModel.liveRunOf` / `runIndicator`, including the
/// distinction that cost the web a bug: the face on a card is its **assignee**
/// — who is on the work, which does not change while somebody else runs it —
/// while the ring belongs to whoever is **running**. About a third of a
/// working board's runs are coordination runs, which execute as `@lead` by
/// construction, so a card that painted the run on its assignee reads
/// "@dev-1 is working" while @lead burns the tokens.
enum RunLabels {
    /// The one run holding this card's slot, if any.
    ///
    /// A card has at most one unsettled run by construction, and "unsettled"
    /// is the question — `settled_at_ms == nil` — never a status match. The
    /// server picks the row the same way.
    static func liveRun(for number: Int64, in runs: [IssueRunInfo]) -> IssueRunInfo? {
        runs.first { $0.number == number && $0.settledAtMs == nil }
    }

    /// The word under a card: `WORKING` / `QUEUED` / `HELD`, or nothing once
    /// the run settles.
    ///
    /// `held` gets its own word rather than folding into `queued`: a queued
    /// run starts on its own when a slot frees, a held one waits on somebody
    /// raising a ceiling, and a card that said "queued" on a board where
    /// every slot was free is the bug this distinction closes.
    static func word(for run: IssueRunInfo?) -> String? {
        switch run?.status {
        case .running: "WORKING"
        case .queued: "QUEUED"
        case .held: "HELD"
        // A requeued row reads `queued` and carries a session; there is no
        // interrupted state on a card's face, deliberately.
        case .done, .failed, .cancelled, .unknown, .none: nil
        }
    }

    /// How long the run has been in the state its word names.
    ///
    /// Running measures from the claim (`started_at_ms`); queued and held
    /// measure from when the row was recorded, because neither has started.
    /// Nothing after it settles.
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

    /// Whether the ring belongs to a second face. A coordination run executes
    /// as the board's lead on a card it is not assigned to, and the card
    /// footer has room for one handle — the assignee's — so the runner
    /// arrives as a face and nothing else.
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
