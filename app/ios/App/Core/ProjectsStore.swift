import Foundation

/// The Projects tab's engine: transport over the active leg, a local mirror so
/// a board paints before the network answers, and the live invalidation lane.
///
/// Two things it deliberately is not.
///
/// It is **not an outbox**. Board writes are never queued for later: a board
/// moves while the phone is away — the driver promotes cards, runs settle,
/// agents comment — so replaying a write authored against a board that has
/// since changed is worse than not having sent it. Offline means writes are
/// disabled, not deferred.
///
/// And the mirror is **REPLACE, never merge**. A refetch overwrites a board
/// wholesale rather than reconciling field by field, because there is no local
/// state worth protecting: everything here is the server's, and a merge would
/// only invent ways for the two to disagree. Compare `SessionIndex`, which
/// does merge — it has optimistic mutations in flight that a stale snapshot
/// must not clobber.
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
            CommentHint.handle(forAgent: agentId, in: team)
        }

        /// The uploaded picture for an agent, if it has one. Looked up here
        /// rather than in each row: a row knows a handle and nothing about the
        /// roster, and the board is the one place holding both.
        func avatarBlobId(forAgent agentId: String) -> String? {
            team.first { $0.id == agentId }?.avatarBlobId
        }
    }

    @Published private(set) var projects: [ProjectInfo] = []
    @Published private(set) var attention: [String: ProjectAttention] = [:]
    @Published private(set) var activity: [String: ProjectActivity] = [:]
    @Published private(set) var boards: [String: Board] = [:]
    /// Parked approval prompts per board, keyed by card number. Never
    /// mirrored: a prompt is a live queue entry with a timeout, and a mirror
    /// that painted one on a cold start would be offering an answer to
    /// something that stopped listening hours ago.
    @Published private(set) var approvalPrompts: [String: [Int64: [IssueApprovalPrompt]]] = [:]
    /// Blocks an AGENT wrote, per board, keyed by card number — the ones that
    /// are a question rather than the operator's own stop order. Read off the
    /// same events pass as the prompts, and unmirrored for the same reason.
    @Published private(set) var blockedQuestions: [String: [Int64: IssueTimeline.PendingQuestion]] =
        [:]
    /// The last refresh could not reach the gateway, so what is on screen is
    /// the mirror. Drives the offline line and disables every write.
    @Published private(set) var isOffline = false
    /// A write's own failure, in the server's words. Cleared when the next one
    /// starts — the board's refusals are sentences an operator reads, and
    /// paraphrasing them client-side is how a phone ends up naming the wrong
    /// ceiling.
    @Published private(set) var writeError: String?

    /// Lazily resolved so constructing the store (an `AppStore` stored
    /// property) never boots the FFI under test.
    private lazy var client: any BayboClientProtocol = clientProvider()
    private let clientProvider: () -> any BayboClientProtocol
    private let supportDirectory: URL

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
    /// order; see `ProjectRecency` for why it is local and why logout takes it.
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
            approvalPrompts: [String: [Int64: [IssueApprovalPrompt]]] = [:],
            blockedQuestions: [String: [Int64: IssueTimeline.PendingQuestion]] = [:]
        ) {
            self.projects = projects
            self.attention = attention
            self.activity = activity
            self.boards = boards
            // Seeded rather than fetched: `refreshWaitingDetails` reads each
            // flagged card's events over the network, which the demo has none
            // of — and without these two the strip can only ever show the
            // failed and unread kinds, i.e. half of what it exists to show.
            self.approvalPrompts = approvalPrompts
            self.blockedQuestions = blockedQuestions
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
    //
    // The FFI records are not `Codable` (UniFFI generates `Equatable` and
    // `Hashable` only), and the on-disk shape should not be the transport's
    // anyway — a gateway field that moves must not invalidate a mirror the
    // user already has. So the mirror carries its own structs, decoded
    // leniently, exactly as `DeckStore` does for its cards.

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

    /// Drop every board this gateway owns. Called on logout and rebind — the
    /// boards belong to the departing gateway, and a mirror that outlived one
    /// is a board from somebody else's account.
    static func removeMirror(in directory: URL = SessionIndex.supportDirectory()) {
        let fm = FileManager.default
        try? fm.removeItem(at: directory.appendingPathComponent("projects.json"))
        // The open-order stamps go with the boards: a project id that meant
        // one board under this gateway means nothing under the next.
        ProjectRecency.remove(in: directory)
        // And every cached card: one belongs to the gateway that served it.
        IssueStore.removeMirrors(in: directory)
        guard let names = try? fm.contentsOfDirectory(atPath: directory.path) else { return }
        for name in names where name.hasPrefix("board-") && name.hasSuffix(".json") {
            try? fm.removeItem(at: directory.appendingPathComponent(name))
        }
    }

    // MARK: - Reads

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
            // Boards with nothing waiting are ABSENT from `/attention`, not
            // zeroed, so the map is rebuilt rather than merged — otherwise a
            // board that just went quiet keeps yesterday's count forever.
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
            boards[projectId] = Board(
                issues: issues, runs: runs, team: team,
                fetchedAtMs: Int64(Date().timeIntervalSince1970 * 1000))
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

    /// Hold refreshes while a row's swipe panel is open: a refetch mid-gesture
    /// re-sorts the list under the thumb, and the server promotes cards on its
    /// own every few seconds, so this is not a rare race.
    func holdRefreshes(_ held: Bool) {
        heldForGesture = held
        guard !held else { return }
        let missed = missedWhileHeld
        missedWhileHeld.removeAll()
        for projectId in missed { scheduleBoardRefresh(projectId) }
    }

    // MARK: - Live invalidation

    /// A board moved.
    ///
    /// **Every scope means the board is dirty.** A move emits no board-scope
    /// frame at all — it records a timeline entry, and entering In Progress a
    /// run — so a client that refetched only on `board` would miss exactly the
    /// change it most needs to draw. The scope is worth carrying only so a
    /// card page can ignore what belongs to another number.
    func projectChanged(projectId: String, scope: String, issueNumber: UInt32?) {
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

    /// Run one board write with the mirror as its undo.
    ///
    /// `apply` moves the local board immediately so the press lands, `call`
    /// does the write, and a failure restores the board **exactly as it was**
    /// — the snapshot, never the inverse of the optimistic edit, which is the
    /// same discipline `SessionIndex` rolls back by. The server's own sentence
    /// is surfaced verbatim: the board's refusals name which ceiling, which
    /// block, which card holds the slot, and a paraphrase loses the only part
    /// the operator can act on.
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
            // The demo board keeps its own writes. Without this every press
            // here reaches a gateway that is not there, fails, and rolls back —
            // which would leave the move, undo, assign and approve flows with
            // no way to be driven headlessly at all.
            if isDemo {
                if var board = boards[projectId] {
                    apply(&board)
                    boards[projectId] = board
                }
                writeError = nil
                return true
            }
        #endif
        let snapshot = boards[projectId]
        if var board = snapshot {
            apply(&board)
            boards[projectId] = board
        }
        writeError = nil
        do {
            try await call(client)
            // The write's own effect arrives as a `ProjectChanged`, but a
            // refetch is scheduled regardless: a failed timeline append never
            // fails the thing it describes, so the frame is not a guarantee.
            scheduleBoardRefresh(projectId)
            scheduleRootRefresh()
            return true
        } catch {
            boards[projectId] = snapshot
            writeError = Self.message(from: error)
            return false
        }
    }

    func clearWriteError() { writeError = nil }

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
    //
    // Each of these is one press on the board, and each is here rather than at
    // its call site for the same reason: every one of them composes something
    // the board owns — the destination column's whole order, which counts a
    // cancelled card, what a local edit does to the mirror — and a second
    // caller composing it again would compose it differently.

    /// Move a card, sending the destination column's FULL order.
    ///
    /// The card goes at the END of the destination, which is what a phone move
    /// can honestly promise: there is no drag, so there is no position the
    /// operator chose. Cancelled cards stay in the order — they are still rows
    /// in that column, and dropping them here would renumber them away.
    @discardableResult
    func move(board projectId: String, issue number: Int64, to status: IssueStatus) async -> Bool {
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

    /// Start another attempt on a card whose last run failed.
    @discardableResult
    func retryRun(board projectId: String, issue number: Int64) async -> Bool {
        await write(board: projectId) { client in
            _ = try await client.projectRunRetry(projectId: projectId, number: number)
        }
    }

    /// Answer one parked approval prompt.
    ///
    /// A 404 is treated as success: the live queue is the truth, and a prompt
    /// that timed out or was answered from another surface is gone rather than
    /// broken. The refetch that follows is what corrects the screen.
    @discardableResult
    func resolveApproval(
        board projectId: String, issue number: Int64, callId: String,
        decision: IssueApprovalDecision
    ) async -> Bool {
        let answered = await write(board: projectId) { client in
            try await client.projectIssueApprovalResolve(
                projectId: projectId, number: number, callId: callId, decision: decision)
        }
        if !answered, Self.readsAsGone(writeError) {
            writeError = "Closed — it timed out, or it was already answered."
            scheduleBoardRefresh(projectId)
        }
        return answered
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

    /// What the Waiting strip needs that the board's own rows cannot say: the
    /// parked prompts, and which blocks are an agent ASKING something.
    ///
    /// Both are read from a card's `events`, which is why one pass fetches
    /// them together — asking twice would double a cost that is already the
    /// strip's whole expense. Bounded by the two flags: only cards marked
    /// `approval_pending` or carrying a `blocked_reason` are fetched, so a
    /// board with nothing waiting costs nothing at all. The fetches are
    /// concurrent, because a board that parked four prompts should not take
    /// four round trips to say so.
    func refreshWaitingDetails(board projectId: String) async {
        guard !isDemo else { return }
        let flagged = (boards[projectId]?.issues ?? [])
            .filter { $0.cancelledAtMs == nil }
            .filter { $0.approvalPending || $0.blockedReason != nil }
        guard !flagged.isEmpty else {
            approvalPrompts[projectId] = [:]
            blockedQuestions[projectId] = [:]
            return
        }
        let client = self.client
        var prompts: [Int64: [IssueApprovalPrompt]] = [:]
        var questions: [Int64: IssueTimeline.PendingQuestion] = [:]
        await withTaskGroup(
            of: (Int64, [IssueApprovalPrompt], IssueTimeline.PendingQuestion?).self
        ) { group in
            for issue in flagged {
                let number = issue.number
                let blockedReason = issue.blockedReason
                group.addTask {
                    guard
                        let json = try? await client.projectIssueEvents(
                            projectId: projectId, number: number),
                        let events = try? IssueEvent.decodeList(json)
                    else { return (number, [], nil) }
                    return (
                        number,
                        IssueTimeline.pendingApprovals(in: events),
                        IssueTimeline.agentQuestion(blockedReason: blockedReason, events: events)
                    )
                }
            }
            for await (number, found, question) in group {
                if !found.isEmpty { prompts[number] = found }
                if let question { questions[number] = question }
            }
        }
        approvalPrompts[projectId] = prompts
        blockedQuestions[projectId] = questions
    }

    private static func patch(
        pinned: Bool? = nil, assignee: StringPatch = .keep, blockedReason: StringPatch = .keep
    ) -> IssuePatch {
        IssuePatch(
            title: nil, description: nil, attachments: nil, priority: nil, assignee: assignee,
            blockedReason: blockedReason, cancelled: nil, parent: nil, stage: nil, pinned: pinned)
    }

    /// Whether the server's refusal was "that prompt no longer exists". The
    /// gateway answers a stale `call_id` with a 404 whose body says so, and
    /// `BayboError.Other` carries that sentence verbatim.
    private static func readsAsGone(_ message: String?) -> Bool {
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

/// Where the connection-global board invalidations land.
///
/// Named `…Relay` rather than `ProjectSinkImpl` deliberately: UniFFI generates
/// a class of exactly that name for every `with_foreign` trait, and a
/// same-named class collides. Calls arrive on the core's tokio workers, so
/// every one hops to the main actor before touching the store.
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
                // Everything else that might be on screen — an open card, a run
                // sheet — hears it here rather than through the boards store,
                // which has no business knowing they exist.
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
