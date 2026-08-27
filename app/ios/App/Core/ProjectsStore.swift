import Foundation

@MainActor
final class ProjectsStore: ObservableObject {
    /// One board, as far as the phone paints it.
    struct Board: Equatable {
        var issues: [IssueInfo] = []
        /// Unsettled runs across the board — the card faces' run word. These
        /// carry no cost fields; the per-card log does.
        var runs: [IssueRunInfo] = []
        var team: [TeamMemberInfo] = []
        var fetchedAtMs: Int64 = 0

        func issues(in status: IssueStatus) -> [IssueInfo] {
            issues.filter { $0.status == status }
        }

        func liveRun(for number: Int64) -> IssueRunInfo? {
            RunLabels.liveRun(for: number, in: runs)
        }

        func handle(forAgent agentId: String) -> String {
            AgentHandles.handle(forAgent: agentId, in: team)
        }

        func avatarBlobId(forAgent agentId: String) -> String? {
            team.first { $0.id == agentId }?.avatarBlobId
        }
    }

    @Published private(set) var projects: [ProjectInfo] = []
    @Published private(set) var attention: [String: ProjectAttention] = [:]
    @Published private(set) var activity: [String: ProjectActivity] = [:]
    @Published private(set) var boards: [String: Board] = [:]
    /// Answerable prompts are live-only state; historical timeline entries are
    /// never allowed to re-arm controls from the disk mirror.
    @Published private(set) var approvalPrompts: [String: [Int64: [IssueApprovalPrompt]]] = [:]
    /// The last refresh could not reach the gateway, so what is on screen is
    /// the mirror. Drives the offline line and disables every write.
    @Published private(set) var isOffline = false
    @Published private(set) var writeError: String?

    /// Lazily resolved so constructing the store (an `AppStore` stored
    /// property) never boots the FFI under test.
    private lazy var client: any BayboClientProtocol = clientProvider()
    private let clientProvider: () -> any BayboClientProtocol
    private let supportDirectory: URL
    private var boardRevisions: [String: UInt64] = [:]

    /// Board refetches in flight, kept so tests can await them and so a burst
    /// of frames collapses into one fetch per board.
    private(set) var refreshTasks: [String: Task<Void, Never>] = [:]
    private(set) var rootTask: Task<Void, Never>?
    /// Frames arrive in bursts — a single move writes a timeline entry and a
    /// run row — so a refetch waits this long for the burst to finish.
    static let invalidationDebounce = Duration.milliseconds(300)
    /// While a row's swipe panel is open, a refetch would re-sort the list
    /// under the thumb. Held refreshes are applied on release.
    private var heldForGesture = false
    private var missedWhileHeld: Set<String> = []
    /// When each board was last opened on THIS phone. Drives the cards root's
    /// order; see `ProjectRecency` for why it is local. Its server namespace
    /// keeps the values from crossing bindings.
    private let recency: ProjectRecency

    init(
        supportDirectory: URL = SessionIndex.supportDirectory(),
        clientProvider: @escaping () -> any BayboClientProtocol = { Baybo.client }
    ) {
        self.supportDirectory = supportDirectory
        self.clientProvider = clientProvider
        recency = ProjectRecency(directory: supportDirectory)
        #if DEBUG
            if Self.demoRequested {
                seedDemo()
                return
            }
        #endif
        loadMirror()
    }

    #if DEBUG
        /// The one door `-baybo-demo-projects` writes through, so the published
        /// properties keep their `private(set)` for every other caller.
        func installDemo(
            projects: [ProjectInfo], attention: [String: ProjectAttention],
            activity: [String: ProjectActivity], boards: [String: Board],
            approvalPrompts: [String: [Int64: [IssueApprovalPrompt]]] = [:]
        ) {
            self.projects = projects
            self.attention = attention
            self.activity = activity
            self.boards = boards
            self.approvalPrompts = approvalPrompts
        }
    #endif

    /// True when the store is serving canned data and must not touch the
    /// network or the disk. Always false in a release build.
    private var isDemo: Bool {
        #if DEBUG
            return Self.demoRequested
        #else
            return false
        #endif
    }

    // MARK: - Mirror
    // The on-disk format is version-tolerant and independent of UniFFI DTOs.

    private var rootMirrorURL: URL { supportDirectory.appendingPathComponent("projects.json") }
    private func boardMirrorURL(_ projectId: String) -> URL? {
        // A project id reaches the filesystem here, so it may not be allowed
        // to name a path: `..` would put the mirror anywhere the app can write.
        guard !projectId.isEmpty, !projectId.contains("/"), !projectId.contains(".") else {
            return nil
        }
        return supportDirectory.appendingPathComponent("board-\(projectId).json")
    }

    private func loadMirror() {
        if let data = try? Data(contentsOf: rootMirrorURL),
            let root = try? JSONDecoder().decode(RootMirror.self, from: data)
        {
            projects = root.projects.map(\.info)
            attention = Dictionary(
                uniqueKeysWithValues: root.attention.map { ($0.projectId, $0.info) })
            activity = Dictionary(
                uniqueKeysWithValues: root.activity.map { ($0.projectId, $0.info) })
            for project in projects {
                guard let url = boardMirrorURL(project.id),
                    let data = try? Data(contentsOf: url),
                    let board = try? JSONDecoder().decode(BoardMirror.self, from: data)
                else { continue }
                boards[project.id] = board.board
            }
        }
    }

    private func persistRoot() {
        let mirror = RootMirror(
            projects: projects.map(ProjectMirror.init(info:)),
            attention: attention.map { AttentionMirror(projectId: $0.key, info: $0.value) },
            activity: activity.map { ActivityMirror(projectId: $0.key, info: $0.value) }
        )
        guard let data = try? JSONEncoder().encode(mirror) else { return }
        try? data.write(to: rootMirrorURL, options: .atomic)
    }

    private func persistBoard(_ projectId: String) {
        guard let url = boardMirrorURL(projectId), let board = boards[projectId],
            let data = try? JSONEncoder().encode(BoardMirror(board: board))
        else { return }
        try? data.write(to: url, options: .atomic)
    }

    @discardableResult
    private func replaceBoard(_ board: Board?, projectId: String) -> UInt64 {
        let revision = boardRevisions[projectId, default: 0] &+ 1
        boardRevisions[projectId] = revision
        if let board {
            boards[projectId] = board
        } else {
            boards.removeValue(forKey: projectId)
        }
        return revision
    }

    static func removeMirror(in directory: URL = SessionIndex.supportDirectory()) {
        let fm = FileManager.default
        try? fm.removeItem(at: directory.appendingPathComponent("projects.json"))
        // The open-order stamps go with the boards: a project id that meant
        // one board under this gateway means nothing under the next.
        ProjectRecency.remove(in: directory)
        // And every cached card: one belongs to the gateway that served it.
        IssueStore.removeMirrors(in: directory)
        IssueCommentOutbox.deleteAll(in: directory)
        guard let names = try? fm.contentsOfDirectory(atPath: directory.path) else { return }
        for name in names where name.hasPrefix("board-") && name.hasSuffix(".json") {
            try? fm.removeItem(at: directory.appendingPathComponent(name))
        }
    }

    // MARK: - Reads

    func issueSeed(projectId: String, number: Int64) -> IssueStore.Seed? {
        guard let board = boards[projectId],
            let issue = board.issues.first(where: { $0.number == number })
        else { return nil }
        return IssueStore.Seed(
            issue: issue,
            runs: board.runs.filter { $0.number == number },
            team: board.team,
            children: board.issues.filter { $0.parent == number })
    }

    /// The cards root: every board, what each is burning today, and what is
    /// waiting on the operator.
    func refreshRoot() async {
        guard !isDemo else { return }
        do {
            let projects = try await client.projectList(includeArchived: true)
            let attention = try await client.projectsAttention()
            let activity = try await client.projectsActivity(
                sinceMs: BudgetMeter.dayStartMs())
            self.projects = projects
            self.attention = Dictionary(
                uniqueKeysWithValues: attention.map { ($0.projectId, $0) })
            self.activity = Dictionary(uniqueKeysWithValues: activity.map { ($0.projectId, $0) })
            isOffline = false
            persistRoot()
        } catch {
            isOffline = true
            NSLog("projects: root refresh failed: %@", String(describing: error))
        }
    }

    /// One board, wholesale.
    func refreshBoard(_ projectId: String) async {
        guard !isDemo else { return }
        do {
            let issues = try await client.projectIssues(projectId: projectId)
            let runs = try await client.projectActiveRuns(projectId: projectId)
            let team = try await client.projectTeam(projectId: projectId)
            replaceBoard(
                Board(
                    issues: issues, runs: runs, team: team,
                    fetchedAtMs: Int64(Date().timeIntervalSince1970 * 1000)),
                projectId: projectId)
            isOffline = false
            persistBoard(projectId)
        } catch {
            isOffline = true
            NSLog("projects: board refresh failed: %@", String(describing: error))
        }
    }

    /// Refetch the board, coalescing a burst of frames into one call.
    func scheduleBoardRefresh(_ projectId: String) {
        guard !heldForGesture else {
            missedWhileHeld.insert(projectId)
            return
        }
        refreshTasks[projectId]?.cancel()
        refreshTasks[projectId] = Task { [weak self] in
            try? await Task.sleep(for: Self.invalidationDebounce)
            guard !Task.isCancelled else { return }
            await self?.refreshBoard(projectId)
        }
    }

    func scheduleRootRefresh() {
        rootTask?.cancel()
        rootTask = Task { [weak self] in
            try? await Task.sleep(for: Self.invalidationDebounce)
            guard !Task.isCancelled else { return }
            await self?.refreshRoot()
        }
    }

    func holdRefreshes(_ held: Bool) {
        heldForGesture = held
        guard !held else { return }
        let missed = missedWhileHeld
        missedWhileHeld.removeAll()
        for projectId in missed { scheduleBoardRefresh(projectId) }
    }

    // MARK: - Live invalidation

    func projectChanged(projectId: String, scope: String, issueNumber: UInt32?) {
        // Every scope can change derived board/root state; scope only narrows
        // which open card also needs a refresh.
        scheduleBoardRefresh(projectId)
        // The counts move with the board, and an answered approval emits no
        // frame of its own.
        scheduleRootRefresh()
    }

    /// The gateway dropped a broadcast, so whatever is on screen is suspect.
    func invalidationsMayHaveBeenDropped() {
        for projectId in boards.keys { scheduleBoardRefresh(projectId) }
        scheduleRootRefresh()
    }

    // MARK: - Writes

    @discardableResult
    func write(
        board projectId: String,
        apply: (inout Board) -> Void = { _ in },
        call: @escaping (any BayboClientProtocol) async throws -> Void
    ) async -> Bool {
        guard !isOffline else {
            writeError = "Offline — this board takes writes again when the connection is back."
            return false
        }
        #if DEBUG
            if isDemo {
                if var board = boards[projectId] {
                    apply(&board)
                    replaceBoard(board, projectId: projectId)
                }
                writeError = nil
                return true
            }
        #endif
        let snapshot = boards[projectId]
        var optimisticRevision: UInt64?
        if var board = snapshot {
            apply(&board)
            optimisticRevision = replaceBoard(board, projectId: projectId)
        }
        writeError = nil
        do {
            try await call(client)
            scheduleBoardRefresh(projectId)
            scheduleRootRefresh()
            return true
        } catch {
            // Roll back only our own optimistic revision. A newer refresh or
            // write wins; otherwise refetch instead of restoring stale state.
            if let optimisticRevision,
                boardRevisions[projectId] == optimisticRevision
            {
                replaceBoard(snapshot, projectId: projectId)
            } else if optimisticRevision != nil {
                await refreshBoard(projectId)
            }
            writeError = Self.message(from: error)
            return false
        }
    }

    func clearWriteError() { writeError = nil }

    func budgetMeter(board projectId: String) -> BudgetMeter.Meter? {
        guard let project = projects.first(where: { $0.id == projectId }) else { return nil }
        let activity = activity[projectId]
        return BudgetMeter.meter(
            burnMicros: activity?.burnMicros ?? 0, burnTokens: activity?.burnTokens ?? 0,
            limitMicros: project.dailyBudgetMicros, limitTokens: project.dailyBudgetTokens)
    }

    // MARK: - Order

    /// The cards root's order: most recently opened on this phone first, then
    /// everything never opened here (in the server's own order).
    func inRecencyOrder(_ projects: [ProjectInfo]) -> [ProjectInfo] {
        recency.ordered(projects)
    }

    /// Stamp a board as opened. Called from the one place a board is entered,
    /// so a second entry point cannot quietly skip it.
    func recordOpened(_ projectId: String) {
        recency.record(projectId)
        // The list is @Published-driven; nothing about `projects` changed, so
        // the reorder has to be announced.
        objectWillChange.send()
    }

    // MARK: - The board's verbs
    // Keep multi-write board rules here so every caller uses the same ordering.

    @discardableResult
    func move(board projectId: String, issue number: Int64, to status: IssueStatus) async -> Bool {
        // The API replaces the destination's full persisted order, including
        // cancelled cards; the phone appends the moved card to that order.
        let destination = (boards[projectId]?.issues ?? [])
            .filter { $0.status == status && $0.number != number }
            .sorted { $0.position < $1.position }
            .map(\.number)
        return await write(
            board: projectId,
            apply: { board in
                guard let index = board.issues.firstIndex(where: { $0.number == number }) else {
                    return
                }
                board.issues[index] = board.issues[index].with(status: status)
            },
            call: { client in
                _ = try await client.projectIssueMove(
                    projectId: projectId, number: number, status: status,
                    orderedNumbers: destination + [number])
            })
    }

    @discardableResult
    func setPinned(board projectId: String, issue number: Int64, _ pinned: Bool) async -> Bool {
        await write(
            board: projectId,
            apply: { board in
                guard let index = board.issues.firstIndex(where: { $0.number == number }) else {
                    return
                }
                board.issues[index] = board.issues[index].with(pinned: pinned)
            },
            call: { client in
                _ = try await client.projectIssuePatch(
                    projectId: projectId, number: number,
                    patch: Self.patch(pinned: pinned))
            })
    }

    @discardableResult
    func assign(board projectId: String, issue number: Int64, to agentId: String?) async -> Bool {
        await write(
            board: projectId,
            apply: { board in
                guard let index = board.issues.firstIndex(where: { $0.number == number }) else {
                    return
                }
                board.issues[index] = board.issues[index].with(
                    assignee: agentId.map { StringPatch.set(value: $0) } ?? .clear)
            },
            call: { client in
                _ = try await client.projectIssuePatch(
                    projectId: projectId, number: number,
                    patch: Self.patch(
                        assignee: agentId.map { StringPatch.set(value: $0) } ?? .clear))
            })
    }

    @discardableResult
    func setPriority(board projectId: String, issue number: Int64, to priority: IssuePriority)
        async -> Bool
    {
        await write(
            board: projectId,
            apply: { board in
                guard let index = board.issues.firstIndex(where: { $0.number == number }) else {
                    return
                }
                board.issues[index] = board.issues[index].with(priority: priority)
            },
            call: { client in
                _ = try await client.projectIssuePatch(
                    projectId: projectId, number: number,
                    patch: Self.patch(priority: priority))
            })
    }

    @discardableResult
    func setCancelled(board projectId: String, issue number: Int64, _ cancelled: Bool) async
        -> Bool
    {
        // Predict every live surface the server removes, then restore the prompt
        // snapshot if the write is refused.
        let promptSnapshot = approvalPrompts[projectId]
        if cancelled, var prompts = promptSnapshot {
            prompts[number] = nil
            approvalPrompts[projectId] = prompts
        }
        let sent = await write(
            board: projectId,
            apply: { board in
                guard let index = board.issues.firstIndex(where: { $0.number == number }) else {
                    return
                }
                board.issues[index] = board.issues[index].with(cancelled: cancelled)
                if cancelled { board.runs.removeAll { $0.number == number } }
            },
            call: { client in
                _ = try await client.projectIssuePatch(
                    projectId: projectId, number: number,
                    patch: Self.patch(cancelled: cancelled))
            })
        if !sent { approvalPrompts[projectId] = promptSnapshot }
        return sent
    }

    @discardableResult
    func retryRun(board projectId: String, issue number: Int64) async -> Bool {
        await write(
            board: projectId,
            apply: { board in
                guard let index = board.issues.firstIndex(where: { $0.number == number }) else {
                    return
                }
                board.issues[index] = board.issues[index].with(lastRunFailed: false)
            },
            call: { client in
                _ = try await client.projectRunRetry(projectId: projectId, number: number)
            })
    }

    @discardableResult
    func resolveApproval(
        board projectId: String, issue number: Int64, callId: String,
        decision: IssueApprovalDecision
    ) async -> Bool {
        let snapshot = approvalPrompts[projectId]
        retirePrompt(board: projectId, issue: number, callId: callId)

        let answered = await write(board: projectId) { client in
            try await client.projectIssueApprovalResolve(
                projectId: projectId, number: number, callId: callId, decision: decision)
        }
        guard !answered else { return true }
        if Self.readsAsGone(writeError) {
            // Gone is gone: a prompt that timed out or was answered elsewhere
            // should stay off the strip, so the optimistic removal STANDS.
            writeError = "Closed — it timed out, or it was already answered."
            scheduleBoardRefresh(projectId)
        } else {
            approvalPrompts[projectId] = snapshot
        }
        return false
    }

    #if DEBUG
        /// Seed the parked prompts directly. Tests only: the real path reads
        /// them off each flagged card's events, which needs a gateway.
        func seedPrompts(board projectId: String, _ prompts: [Int64: [IssueApprovalPrompt]]) {
            approvalPrompts[projectId] = prompts
        }
    #endif

    private func retirePrompt(board projectId: String, issue number: Int64, callId: String) {
        guard var forBoard = approvalPrompts[projectId], var forCard = forBoard[number] else {
            return
        }
        forCard.removeAll { $0.callId == callId }
        // An empty list and an absent key must not both mean "none waiting" —
        // the strip reads absence, so collapse to it.
        forBoard[number] = forCard.isEmpty ? nil : forCard
        approvalPrompts[projectId] = forBoard
    }

    /// Stamp the whole board read. Optimistic, because the number it clears is
    /// the one on the screen the press is on.
    @discardableResult
    func markAllRead(board projectId: String) async -> Bool {
        await write(
            board: projectId,
            apply: { board in
                for index in board.issues.indices {
                    board.issues[index] = board.issues[index].with(unread: 0)
                }
            },
            call: { client in try await client.projectRead(projectId: projectId) })
    }

    func refreshApprovalPrompts(board projectId: String) async {
        guard !isDemo else { return }
        let flagged = (boards[projectId]?.issues ?? [])
            .filter { $0.cancelledAtMs == nil && $0.approvalPending }
        guard !flagged.isEmpty else {
            approvalPrompts[projectId] = [:]
            return
        }
        // There is no board-level prompt route, so fetch only flagged cards and
        // do those independent timeline reads concurrently.
        let client = self.client
        var prompts: [Int64: [IssueApprovalPrompt]] = [:]
        await withTaskGroup(of: (Int64, [IssueApprovalPrompt]).self) { group in
            for issue in flagged {
                let number = issue.number
                group.addTask {
                    guard
                        let json = try? await client.projectIssueEvents(
                            projectId: projectId, number: number),
                        let events = try? IssueEvent.decodeList(json)
                    else { return (number, []) }
                    return (number, IssueTimeline.pendingApprovals(in: events))
                }
            }
            for await (number, found) in group where !found.isEmpty {
                prompts[number] = found
            }
        }
        approvalPrompts[projectId] = prompts
    }

    private static func patch(
        pinned: Bool? = nil, priority: IssuePriority? = nil, assignee: StringPatch = .keep,
        blockedReason: StringPatch = .keep, cancelled: Bool? = nil
    ) -> IssuePatch {
        IssuePatch(
            title: nil, description: nil, attachments: nil, priority: priority,
            assignee: assignee, blockedReason: blockedReason, cancelled: cancelled, parent: nil,
            detachParent: false, stage: nil, pinned: pinned)
    }

    static func readsAsGone(_ message: String?) -> Bool {
        guard let message = message?.lowercased() else { return false }
        return message.contains("404") || message.contains("not found")
    }

    /// The gateway's own words. `BayboError.Other` carries the server's
    /// sentence verbatim; the two typed variants are the transport's.
    static func message(from error: Error) -> String {
        guard let error = error as? BayboError else { return String(describing: error) }
        switch error {
        case .NotConnected, .NotBound:
            return "Offline — this board takes writes again when the connection is back."
        case .InvalidToken:
            return "This device is no longer signed in."
        case let .Other(message):
            return message
        }
    }
}

final class ProjectEventsRelay: ProjectSink {
    private let store: @MainActor @Sendable () -> ProjectsStore?

    init(store: @escaping @MainActor @Sendable () -> ProjectsStore?) {
        self.store = store
    }

    func onProjectChanged(projectId: String, scope: String, issueNumber: UInt32?) {
        let store = self.store
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                store()?.projectChanged(
                    projectId: projectId, scope: scope, issueNumber: issueNumber)
                ProjectInvalidations.shared.publish(
                    projectId: projectId, scope: scope,
                    issueNumber: issueNumber.map(Int64.init))
            }
        }
    }

    func onProjectStale() {
        let store = self.store
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                store()?.invalidationsMayHaveBeenDropped()
                ProjectInvalidations.shared.publishStale()
            }
        }
    }

}
