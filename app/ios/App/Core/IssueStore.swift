import Foundation

/// One project card, as the card page renders it.
///
/// A separate type from `ProjectsStore` rather than a slice of it, and for the
/// reason the board's own writes are in the store: this holds a WEBVIEW's
/// worth of state — a bridge, an editing mode, a bottom inset — none of which
/// a board screen has any business carrying, and all of which would need a
/// per-card key inside a store that is keyed by board.
///
/// Attachments are not reimplemented: `TranscriptMedia` is the same engine the
/// chat transcript uses, so a file card on a project card behaves exactly as
/// it does in a conversation.
@MainActor
final class IssueStore: ObservableObject, WebMediaTarget {
    let projectId: String
    let number: Int64

    /// The card as last fetched. Nil until the first read lands — the page
    /// shows its own loading line rather than an empty card.
    @Published private(set) var issue: IssueInfo?
    @Published private(set) var events: [IssueEvent] = []
    /// The timeline's raw `{"items":[…]}` envelope, kept alongside the
    /// decoded form. The page renders the gateway's own shape, so it gets the
    /// bytes verbatim — re-encoding through a Swift mirror would be a third
    /// place every new event kind has to be taught about.
    private var eventsJson = "{\"items\":[]}"
    @Published private(set) var runs: [IssueRunInfo] = []
    @Published private(set) var team: [TeamMemberInfo] = []
    @Published private(set) var children: [IssueInfo] = []
    /// The last write's failure, in the server's own words.
    @Published private(set) var writeError: String?
    /// The description editor is open. Native owns the bar; the web owns the
    /// textarea.
    @Published var editing = false
    @Published private(set) var isRefreshing = false

    @Published var filePreview: FilePreview?
    @Published var fileShare: FilePreview?
    @Published var viewedImage: ViewedImage?
    @Published var videoPlayback: VideoPlayback?

    private let client: any BayboClientProtocol
    private weak var bridge: IssueBridge?
    /// The card is stamped read once its timeline has rendered, and once —
    /// re-stamping per delivery would spend a round trip per comment.
    private var stampedRead = false
    private var invalidations: ProjectInvalidations.Token?

    private lazy var media: TranscriptMedia = {
        let media = TranscriptMedia(client: client)
        media.onPreview = { [weak self] in self?.filePreview = $0 }
        media.onShare = { [weak self] in self?.fileShare = $0 }
        media.onViewImage = { [weak self] in self?.viewedImage = $0 }
        media.onPlayVideo = { [weak self] in self?.videoPlayback = $0 }
        return media
    }()

    init(
        projectId: String, number: Int64,
        client: any BayboClientProtocol = Baybo.client
    ) {
        self.projectId = projectId
        self.number = number
        self.client = client
    }

    func attach(_ bridge: IssueBridge) {
        self.bridge = bridge
        media.attach(bridge)
        invalidations = ProjectInvalidations.shared.observe { [weak self] change in
            guard let self else { return }
            // A stale broadcast names no board, and every scope means dirty —
            // the card is small enough that refetching it whole beats being
            // right about which part moved.
            guard change.scope == "stale" || change.projectId == self.projectId else { return }
            guard change.issueNumber == nil || change.issueNumber == self.number else { return }
            self.invalidated()
        }
    }

    func detach(_ bridge: IssueBridge) {
        guard self.bridge === bridge else { return }
        invalidations = nil
        media.detach(bridge)
        self.bridge = nil
    }

    // MARK: - Reads

    /// Everything the page draws, in one pass.
    ///
    /// Four calls rather than one because the gateway has four routes and no
    /// composite: the card, its timeline, its runs, and the board's team (for
    /// the handle map — the DTOs carry profile ids). Concurrent, because they
    /// are independent and a card that took four serial round trips to open
    /// would feel like it.
    func refresh() async {
        isRefreshing = true
        defer { isRefreshing = false }
        async let issue = try? client.projectIssueGet(projectId: projectId, number: number)
        async let eventsJson = try? client.projectIssueEvents(
            projectId: projectId, number: number)
        async let runs = try? client.projectIssueRuns(projectId: projectId, number: number)
        async let team = try? client.projectTeam(projectId: projectId)
        async let siblings = try? client.projectIssues(projectId: projectId)

        if let fetched = await issue { self.issue = fetched }
        if let json = await eventsJson {
            self.eventsJson = json
            events = (try? IssueEvent.decodeList(json)) ?? []
        }
        // The log carries totals beside the rows; the page prints the rows and
        // the totals belong to a screen that does not exist yet (P7).
        if let fetched = await runs { self.runs = fetched.runs }
        if let fetched = await team { self.team = fetched }
        // Children come from the board: the card DTO carries a done/total
        // count and no list at all.
        if let fetched = await siblings {
            children = fetched.filter { $0.parent == number }
        }
        deliver()
    }

    /// A frame said this card changed. Scoped refetches would be four separate
    /// partial paths through the same four routes; the card is small enough
    /// that refetching it whole is cheaper than being right about which part
    /// moved.
    func invalidated() {
        Task { await refresh() }
    }

    // MARK: - Writes

    @discardableResult
    private func write(_ call: @escaping (any BayboClientProtocol) async throws -> Void) async
        -> Bool
    {
        writeError = nil
        do {
            try await call(client)
            await refresh()
            return true
        } catch {
            writeError = ProjectsStore.message(from: error)
            return false
        }
    }

    func setDescription(_ text: String) {
        Task {
            await write { [projectId, number] client in
                _ = try await client.projectIssuePatch(
                    projectId: projectId, number: number,
                    patch: Self.patch(description: text))
            }
        }
    }

    func comment(_ text: String) {
        Task {
            await write { [projectId, number] client in
                _ = try await client.projectIssueComment(
                    projectId: projectId, number: number, text: text, attachments: [])
            }
        }
    }

    func resolveApproval(callId: String, decision: IssueApprovalDecision) {
        Task {
            await write { [projectId, number] client in
                try await client.projectIssueApprovalResolve(
                    projectId: projectId, number: number, callId: callId, decision: decision)
            }
        }
    }

    func stopRun() {
        Task {
            await write { [projectId, number] client in
                _ = try await client.projectRunCancel(projectId: projectId, number: number)
            }
        }
    }

    func retryRun() {
        Task {
            await write { [projectId, number] client in
                _ = try await client.projectRunRetry(projectId: projectId, number: number)
            }
        }
    }

    /// Lift a block, handing the parked run back out.
    ///
    /// Sent AFTER the comment that answers it, never before: the unblock door
    /// rebuilds the run's brief from what the card says at that moment, so
    /// lifting first restarts the agent without the answer it stopped for.
    func unblock() {
        Task {
            await write { [projectId, number] client in
                _ = try await client.projectIssuePatch(
                    projectId: projectId, number: number,
                    patch: IssuePatch(
                        title: nil, description: nil, attachments: nil, priority: nil,
                        assignee: .keep, blockedReason: .clear, cancelled: nil, parent: nil,
                        stage: nil, pinned: nil))
            }
        }
    }

    func clearWriteError() { writeError = nil }

    /// Stamp the card read. Called from the page's own "I rendered" message —
    /// never on the way in, because a card whose timeline threw has not been
    /// read by anybody.
    func markRendered() {
        guard !stampedRead else { return }
        stampedRead = true
        Task { [projectId, number] in
            try? await client.projectIssueRead(projectId: projectId, number: number)
            AppStore.shared?.projectsStore.scheduleRootRefresh()
        }
    }

    // MARK: - Derived

    var liveRun: IssueRunInfo? {
        runs.first { $0.settledAtMs == nil }
    }

    var pendingApprovals: [IssueApprovalPrompt] {
        IssueTimeline.pendingApprovals(in: events)
    }

    /// What sending a comment will do, said before it is sent — the third
    /// mirror of a rule that lives in Rust (see `CommentHint`).
    var commentHint: String {
        guard let issue else { return "" }
        return CommentHint.text(
            status: issue.status,
            assigneeHandle: issue.assignee.map(handle(forAgent:)),
            cancelled: issue.cancelledAtMs != nil,
            blockedReason: issue.blockedReason,
            liveRunStatus: liveRun?.status)
    }

    func handle(forAgent agentId: String) -> String {
        CommentHint.handle(forAgent: agentId, in: team)
    }

    // MARK: - The page

    /// Hand the page everything at once. A full replacement rather than a
    /// merge: a card is small, its parts move together — a comment writes a
    /// timeline entry AND bumps the card — so there is nothing a field-by-field
    /// merge would protect and one more way for the two to disagree if it
    /// tried.
    private func deliver() {
        guard let issue else { return }
        bridge?.deliver(
            issue: issue, eventsJson: eventsJson, runs: runs,
            handles: Dictionary(team.map { ($0.id, $0.handle) }, uniquingKeysWith: { a, _ in a }),
            children: children)
    }

    /// Re-send whatever is loaded. Called when the page reports `ready` after
    /// a crash reload — its React tree is new and holds nothing.
    func redeliver() { deliver() }

    // MARK: - Attachments
    //
    // Every one of these is the shared engine's, unchanged. A file card on a
    // project card behaves exactly as it does in a conversation because it IS
    // the same code on both ends.

    func requestBlob(id: Int, blobId: String) { media.requestBlob(id: id, blobId: blobId) }
    func queryFileState(blobId: String) { media.queryFileState(blobId: blobId) }
    func downloadFile(blobId: String) { media.downloadFile(blobId: blobId) }
    func previewFile(blobId: String, filename: String, mimeType: String) {
        media.previewFile(blobId: blobId, filename: filename, mimeType: mimeType)
    }
    func shareFile(blobId: String, filename: String, mimeType: String) {
        media.shareFile(blobId: blobId, filename: filename, mimeType: mimeType)
    }
    func viewImage(blobId: String, filename: String, mimeType: String) {
        media.viewImage(blobId: blobId, filename: filename, mimeType: mimeType)
    }
    func playVideo(blobId: String, filename: String, mimeType: String) {
        media.playVideo(blobId: blobId, filename: filename, mimeType: mimeType)
    }
    func requestVideoPoster(id: Int, blobId: String, filename: String, mimeType: String) {
        media.requestVideoPoster(id: id, blobId: blobId, filename: filename, mimeType: mimeType)
    }
    func audioToggle(blobId: String, filename: String, mimeType: String) {
        media.audioToggle(blobId: blobId, filename: filename, mimeType: mimeType)
    }
    func audioSeek(blobId: String, position: Double) {
        media.audioSeek(blobId: blobId, position: position)
    }
    func queryAudioState(blobId: String) { media.queryAudioState(blobId: blobId) }

    private static func patch(description: String) -> IssuePatch {
        IssuePatch(
            title: nil, description: description, attachments: nil, priority: nil,
            assignee: .keep, blockedReason: .keep, cancelled: nil, parent: nil, stage: nil,
            pinned: nil)
    }
}
