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
    private let supportDirectory: URL
    private weak var bridge: IssueBridge?
    /// Everything on screen came off DISK and no live fetch has landed yet.
    ///
    /// Load-bearing, not cosmetic: a mirrored card may name a prompt that
    /// timed out hours ago and a run that has long since settled, and both of
    /// those drive controls that ACT. Content paints from the mirror; anything
    /// that presses a live queue waits for the network.
    @Published private(set) var isFromMirror = false
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
        client: any BayboClientProtocol = Baybo.client,
        supportDirectory: URL = SessionIndex.supportDirectory()
    ) {
        self.projectId = projectId
        self.number = number
        self.client = client
        self.supportDirectory = supportDirectory
        loadMirror()
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

        let fetchedIssue = await issue
        if let fetchedIssue { self.issue = fetchedIssue }
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
        // THIS fetch's own answer, not `self.issue` — that is non-nil the
        // moment a mirror loads, so reading it would arm the live controls off
        // a cached card the network never confirmed.
        if fetchedIssue != nil { isFromMirror = false }
        persistMirror()
        deliver()
    }

    // MARK: - Mirror
    //
    // REPLACE, never merge — the board's rule, for the board's reason: there
    // is no local state here worth protecting, and a merge would only invent
    // ways for the two to disagree.

    private var mirrorURL: URL? {
        // The project id reaches the filesystem, so it may not name a path;
        // the number is an Int64 and cannot.
        guard !projectId.isEmpty, !projectId.contains("/"), !projectId.contains(".") else {
            return nil
        }
        return supportDirectory.appendingPathComponent("issue-\(projectId)-\(number).json")
    }

    private func loadMirror() {
        guard let url = mirrorURL, let data = try? Data(contentsOf: url),
            let mirror = try? JSONDecoder().decode(
                ProjectsStore.IssueContentMirror.self, from: data)
        else { return }
        issue = mirror.issue.info
        eventsJson = mirror.eventsJson
        events = (try? IssueEvent.decodeList(mirror.eventsJson)) ?? []
        runs = mirror.runs.map(\.info)
        team = mirror.team.map(\.info)
        children = mirror.children.map(\.info)
        isFromMirror = true
    }

    private func persistMirror() {
        guard let url = mirrorURL, let issue else { return }
        let mirror = ProjectsStore.IssueContentMirror(
            issue: ProjectsStore.IssueMirror(info: issue),
            eventsJson: eventsJson,
            runs: runs.map(ProjectsStore.RunMirror.init(info:)),
            team: team.map(ProjectsStore.TeamMirror.init(info:)),
            children: children.map(ProjectsStore.IssueMirror.init(info:)),
            fetchedAtMs: Int64(Date().timeIntervalSince1970 * 1000))
        guard let data = try? JSONEncoder().encode(mirror) else { return }
        try? data.write(to: url, options: .atomic)
    }

    /// Throw this card's local state away and let the COLD-OPEN path rebuild
    /// it — the chat's escape hatch, applied to a card.
    ///
    /// Deliberately not a new reconciliation routine: a freshly installed
    /// device renders this card correctly off the same server data, so the
    /// reconstruction known to be right is the one a first open runs — no
    /// mirror on disk, a page with no memory, one fetch.
    ///
    /// Three steps and nothing else:
    ///
    /// 1. delete the mirror, so nothing restores;
    /// 2. drop what is in memory, because on THIS page native holds the
    ///    content and pushes it — clearing it is what "a page with no memory"
    ///    means here, where the chat's equivalent state lives in the webview;
    ///    and
    /// 3. reload the document, so every in-memory web latch dies with it
    ///    rather than being enumerated and cleared. A "reset yourself" bridge
    ///    message is deliberately NOT what this is: it could only clear the
    ///    state somebody thought to list, and state that was not cleared when
    ///    it should have been is exactly what the hatch exists to escape.
    ///
    /// One scar the chat carries that this does not: there is no
    /// `discardPersist` here, because the card page never writes the mirror —
    /// native does, after a fetch. The dying document has no persist to
    /// resurrect what step 1 just deleted.
    ///
    /// What it does NOT touch: the board's own mirror (a card is not its
    /// board), and the live approval queue — answering is REST, so a prompt
    /// survives this untouched and is re-derived from the refetched timeline.
    func resync() {
        if let url = mirrorURL {
            try? FileManager.default.removeItem(at: url)
        }
        issue = nil
        events = []
        eventsJson = "{\"items\":[]}"
        runs = []
        children = []
        writeError = nil
        // Nothing is from a mirror any more, because nothing is here at all.
        isFromMirror = false
        stampedRead = false
        bridge?.rebuild()
        Task { await refresh() }
    }

    /// Drop every cached card. Called with the board mirrors on logout — a
    /// card belongs to the gateway that served it.
    static func removeMirrors(in directory: URL = SessionIndex.supportDirectory()) {
        let fm = FileManager.default
        guard let names = try? fm.contentsOfDirectory(atPath: directory.path) else { return }
        for name in names where name.hasPrefix("issue-") && name.hasSuffix(".json") {
            try? fm.removeItem(at: directory.appendingPathComponent(name))
        }
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
        // Latched BEFORE the await so a second `issueRendered` in the same
        // breath cannot send twice, and cleared again if the send failed —
        // otherwise one lost POST leaves the card unread for this screen's
        // whole life, with the unread row sitting on the board behind it and
        // no way to discharge it but Mark all read.
        stampedRead = true
        Task { [projectId, number] in
            do {
                try await client.projectIssueRead(projectId: projectId, number: number)
            } catch {
                stampedRead = false
                NSLog("baybo: mark read #%lld: %@", number, bayboErrorText(error))
                return
            }
            // The board plane too, not just the root's counts: the Waiting
            // strip reads `board.issues[].unread`, which `refreshRoot` never
            // writes. A frame normally does this — `mark_issue_read` emits a
            // timeline invalidation — but a leg that is down or redialing
            // carries no frame while the REST call still lands.
            AppStore.shared?.projectsStore.scheduleBoardRefresh(projectId)
            AppStore.shared?.projectsStore.scheduleRootRefresh()
        }
    }

    // MARK: - Derived

    /// The run holding this card, **once a live answer has landed**.
    ///
    /// Withheld while the page is showing the mirror: a run that was unsettled
    /// when this was written may have finished hours ago, and this drives the
    /// header's Stop — a button that would then be offering to stop something
    /// already over.
    var liveRun: IssueRunInfo? {
        guard !isFromMirror else { return nil }
        return runs.first { $0.settledAtMs == nil }
    }

    /// Prompts parked on this card, **once a live answer has landed**.
    ///
    /// Never from the mirror, for the reason the board refuses to cache them
    /// at all: a prompt is a live queue entry with a 300s timeout, and one
    /// replayed off disk would offer an answer to something that stopped
    /// listening hours ago.
    var pendingApprovals: [IssueApprovalPrompt] {
        guard !isFromMirror else { return [] }
        return IssueTimeline.pendingApprovals(in: events)
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
