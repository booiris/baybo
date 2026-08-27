import Foundation

// MARK: - Mirror shapes
//
// Deliberately separate from the FFI records: this is a file format an
// upgrade has to keep reading, and every field is optional-tolerant so a
// mirror written by an older build still paints.
//
// Lifted out of `ProjectsStore` when a SECOND mirror needed them: a card page
// caches its own content (`IssueContentMirror`), and the card, its runs and
// its team are the same records the board already writes. Two copies of these
// shapes would be two file formats that have to stay identical by hand.

extension ProjectsStore {
    struct RootMirror: Codable {
        var projects: [ProjectMirror] = []
        var attention: [AttentionMirror] = []
        var activity: [ActivityMirror] = []
    }

    struct ProjectMirror: Codable {
        let id: String
        let name: String
        var description: String = ""
        var workdir: String = ""
        var dailyBudgetMicros: Int64?
        var dailyBudgetTokens: Int64?
        var maxParallelIssueRuns: Int64 = 0
        var agentsMayMerge: Bool = false
        var archivedAtMs: Int64?
        var createdAtMs: Int64 = 0
        var updatedAtMs: Int64 = 0

        init(info: ProjectInfo) {
            id = info.id
            name = info.name
            description = info.description
            workdir = info.workdir
            dailyBudgetMicros = info.dailyBudgetMicros
            dailyBudgetTokens = info.dailyBudgetTokens
            maxParallelIssueRuns = info.maxParallelIssueRuns
            agentsMayMerge = info.agentsMayMerge
            archivedAtMs = info.archivedAtMs
            createdAtMs = info.createdAtMs
            updatedAtMs = info.updatedAtMs
        }

        var info: ProjectInfo {
            ProjectInfo(
                id: id, name: name, description: description, workdir: workdir,
                dailyBudgetMicros: dailyBudgetMicros, dailyBudgetTokens: dailyBudgetTokens,
                maxParallelIssueRuns: maxParallelIssueRuns, agentsMayMerge: agentsMayMerge,
                archivedAtMs: archivedAtMs, createdAtMs: createdAtMs, updatedAtMs: updatedAtMs)
        }
    }

    struct AttentionMirror: Codable {
        let projectId: String
        var name: String = ""
        var approvals: UInt32 = 0
        var failed: UInt32 = 0
        var unread: UInt32 = 0

        init(projectId: String, info: ProjectAttention) {
            self.projectId = projectId
            name = info.name
            approvals = info.approvals
            failed = info.failed
            unread = info.unread
        }

        var info: ProjectAttention {
            ProjectAttention(
                projectId: projectId, name: name, approvals: approvals, failed: failed,
                unread: unread)
        }
    }

    struct ActivityMirror: Codable {
        let projectId: String
        var working: UInt32 = 0
        var burnMicros: Int64 = 0
        var burnTokens: Int64 = 0

        init(projectId: String, info: ProjectActivity) {
            self.projectId = projectId
            working = info.working
            burnMicros = info.burnMicros
            burnTokens = info.burnTokens
        }

        var info: ProjectActivity {
            ProjectActivity(
                projectId: projectId, working: working, burnMicros: burnMicros,
                burnTokens: burnTokens)
        }
    }

    struct BoardMirror: Codable {
        var issues: [IssueMirror] = []
        var runs: [RunMirror] = []
        var team: [TeamMirror] = []
        var fetchedAtMs: Int64 = 0

        init(board: Board) {
            issues = board.issues.map(IssueMirror.init(info:))
            runs = board.runs.map(RunMirror.init(info:))
            team = board.team.map(TeamMirror.init(info:))
            fetchedAtMs = board.fetchedAtMs
        }

        var board: Board {
            Board(
                issues: issues.map(\.info), runs: runs.map(\.info), team: team.map(\.info),
                fetchedAtMs: fetchedAtMs)
        }
    }

    struct IssueMirror: Codable {
        let number: Int64
        var projectId: String = ""
        var title: String = ""
        var description: String = ""
        var status: String = "backlog"
        var priority: String = "none"
        var assignee: String?
        var position: Int64 = 0
        var pinned: Bool = false
        var branch: String?
        var blockedReason: String?
        var parent: Int64?
        var filedFrom: Int64?
        var stage: Int64 = 0
        var subIssuesDone: Int64?
        var subIssuesTotal: Int64?
        var unread: Int64 = 0
        var lastRunFailed: Bool = false
        var approvalPending: Bool = false
        var openedByAgent: Bool = false
        var cancelledAtMs: Int64?
        var createdAtMs: Int64 = 0
        var updatedAtMs: Int64 = 0
        /// Absent in mirrors written before a card page existed — the board
        /// never drew a card's files, so it never wrote them. Decodes empty,
        /// which reads as "no attachments" until the next live fetch.
        var attachments: [AttachmentMirror]?

        init(info: IssueInfo) {
            number = info.number
            projectId = info.projectId
            title = info.title
            description = info.description
            status = ProjectsStore.word(info.status)
            priority = ProjectsStore.word(info.priority)
            assignee = info.assignee
            position = info.position
            pinned = info.pinned
            branch = info.branch
            blockedReason = info.blockedReason
            parent = info.parent
            filedFrom = info.filedFrom
            stage = info.stage
            subIssuesDone = info.subIssues?.done
            subIssuesTotal = info.subIssues?.total
            unread = info.unread
            lastRunFailed = info.lastRunFailed
            approvalPending = info.approvalPending
            openedByAgent = info.openedByAgent
            cancelledAtMs = info.cancelledAtMs
            createdAtMs = info.createdAtMs
            updatedAtMs = info.updatedAtMs
            attachments = info.attachments.map(AttachmentMirror.init(info:))
        }

        var info: IssueInfo {
            IssueInfo(
                number: number, projectId: projectId, title: title, description: description,
                attachments: (attachments ?? []).map(\.info),
                status: ProjectsStore.status(status),
                priority: ProjectsStore.priority(priority), assignee: assignee,
                position: position, pinned: pinned, branch: branch,
                blockedReason: blockedReason, parent: parent, filedFrom: filedFrom, stage: stage,
                subIssues: subIssuesDone.flatMap { done in
                    subIssuesTotal.map { SubIssueProgress(done: done, total: $0) }
                },
                unread: unread, lastRunFailed: lastRunFailed, approvalPending: approvalPending,
                openedByAgent: openedByAgent, cancelledAtMs: cancelledAtMs,
                createdAtMs: createdAtMs, updatedAtMs: updatedAtMs)
        }
    }

    struct RunMirror: Codable {
        let number: Int64
        var attempt: Int64 = 0
        var agentId: String = ""
        var status: String = "queued"
        var trigger: String = "started"
        var sessionId: String?
        var error: String?
        var createdAtMs: Int64 = 0
        var startedAtMs: Int64?
        var settledAtMs: Int64?

        init(info: IssueRunInfo) {
            number = info.number
            attempt = info.attempt
            agentId = info.agentId
            status = ProjectsStore.word(info.status)
            trigger = ProjectsStore.word(info.trigger)
            sessionId = info.sessionId
            error = info.error
            createdAtMs = info.createdAtMs
            startedAtMs = info.startedAtMs
            settledAtMs = info.settledAtMs
        }

        var info: IssueRunInfo {
            IssueRunInfo(
                number: number, attempt: attempt, agentId: agentId,
                status: ProjectsStore.runStatus(status), trigger: ProjectsStore.trigger(trigger),
                sessionId: sessionId, error: error, createdAtMs: createdAtMs,
                startedAtMs: startedAtMs, settledAtMs: settledAtMs,
                // Never mirrored: the active-run poll does not price runs, and
                // a mirror that wrote 0 would report free work as fact.
                costMicros: nil, inputTokens: nil, outputTokens: nil)
        }
    }

    struct TeamMirror: Codable {
        let id: String
        var handle: String = ""
        var name: String = ""
        var description: String = ""
        var avatarBlobId: String?
        var framework: String = "baybo"
        var lead: Bool = false
        var createdAtMs: Int64 = 0

        init(info: TeamMemberInfo) {
            id = info.id
            handle = info.handle
            name = info.name
            description = info.description
            avatarBlobId = info.avatarBlobId
            framework = info.framework
            lead = info.lead
            createdAtMs = info.createdAtMs
        }

        var info: TeamMemberInfo {
            TeamMemberInfo(
                id: id, handle: handle, name: name, description: description,
                avatarBlobId: avatarBlobId, framework: framework, llm: nil, model: nil,
                reasoningEffort: nil, lead: lead, hiredBy: nil, createdAtMs: createdAtMs)
        }
    }

    // The mirror stores enums as their wire words, so a build that has never
    // heard of a status still round-trips one it read.
    nonisolated static func word(_ status: IssueStatus) -> String {
        switch status {
        case .backlog: "backlog"
        case .todo: "todo"
        case .inProgress: "in_progress"
        case .review: "review"
        case .done: "done"
        case .unknown: "unknown"
        }
    }

    nonisolated static func status(_ word: String) -> IssueStatus {
        switch word {
        case "backlog": .backlog
        case "todo": .todo
        case "in_progress": .inProgress
        case "review": .review
        case "done": .done
        default: .unknown
        }
    }

    nonisolated static func word(_ priority: IssuePriority) -> String {
        switch priority {
        case .urgent: "urgent"
        case .high: "high"
        case .medium: "medium"
        case .low: "low"
        case .none: "none"
        case .unknown: "unknown"
        }
    }

    nonisolated static func priority(_ word: String) -> IssuePriority {
        switch word {
        case "urgent": .urgent
        case "high": .high
        case "medium": .medium
        case "low": .low
        case "none": .none
        default: .unknown
        }
    }

    nonisolated static func word(_ status: RunStatus) -> String {
        switch status {
        case .held: "held"
        case .queued: "queued"
        case .running: "running"
        case .done: "done"
        case .failed: "failed"
        case .cancelled: "cancelled"
        case .unknown: "unknown"
        }
    }

    nonisolated static func runStatus(_ word: String) -> RunStatus {
        switch word {
        case "held": .held
        case "queued": .queued
        case "running": .running
        case "done": .done
        case "failed": .failed
        case "cancelled": .cancelled
        default: .unknown
        }
    }

    nonisolated static func word(_ trigger: RunTrigger) -> String {
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

    nonisolated static func trigger(_ word: String) -> RunTrigger {
        switch word {
        case "started": .started
        case "assigned": .assigned
        case "retry": .retry
        case "comment": .comment
        case "promoted": .promoted
        case "triage": .triage
        case "stage_barrier": .stageBarrier
        case "review": .review
        case "stalled": .stalled
        case "blocked": .blocked
        case "grooming": .grooming
        case "board_idle": .boardIdle
        default: .unknown
        }
    }

    struct AttachmentMirror: Codable {
        var blobId: String = ""
        var mimeType: String = ""
        var size: UInt32 = 0
        var filename: String?

        init(info: IssueAttachmentInfo) {
            blobId = info.blobId
            mimeType = info.mimeType
            size = info.size
            filename = info.filename
        }

        var info: IssueAttachmentInfo {
            IssueAttachmentInfo(
                blobId: blobId, mimeType: mimeType, size: size, filename: filename)
        }
    }

    /// One card's page, cached so it paints before the network answers.
    ///
    /// **The timeline rides as its raw envelope**, the same bytes the gateway
    /// sent: its only consumer is the webview, and a Swift mirror of it would
    /// be a third place every new event kind has to be taught about.
    ///
    /// The team rides along rather than being read from the board's mirror,
    /// because a card can be opened without its board ever having been —
    /// a `#N` link inside another card's prose is a door straight to it.
    struct IssueContentMirror: Codable {
        var issue: IssueMirror
        var eventsJson: String = "{\"items\":[]}"
        var runs: [RunMirror] = []
        var team: [TeamMirror] = []
        var children: [IssueMirror] = []
        var fetchedAtMs: Int64 = 0
    }
}
