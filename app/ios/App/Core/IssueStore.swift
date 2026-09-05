import Foundation

@MainActor
final class IssueStore: ObservableObject, WebMediaTarget {
    struct Seed {
        let issue: IssueInfo
        let runs: [IssueRunInfo]
        let team: [TeamMemberInfo]
        let children: [IssueInfo]
    }

    struct PageState: Equatable {
        let scrollTop: Double
        let folds: [String: Bool]
    }

    let projectId: String
    let number: Int64

    /// The card as last fetched. Nil until the first read lands — the page
    /// shows its own loading line rather than an empty card.
    @Published private(set) var issue: IssueInfo?
    @Published private(set) var events: [IssueEvent] = []
    private var eventsJson = "{\"items\":[]}"
    /// Live response only. Restoring this from a mirror would move the reader
    /// to an old boundary just as opening the card stamps it read.
    private(set) var firstUnread: String?
    @Published private(set) var runs: [IssueRunInfo] = []
    @Published private(set) var team: [TeamMemberInfo] = []
    @Published private(set) var children: [IssueInfo] = []
    /// The last write's failure, in the server's own words. Drawn as a banner
    /// at the top of the screen, and cleared on entry by every write.
    @Published private(set) var writeError: String?
    @Published var notice: String?
    @Published private(set) var isRefreshing = false

    // MARK: - What the page asks the screen for
    // Store state avoids screen closures that a pooled renderer could retain.

    /// A chip was pressed: `status` / `priority` / `assignee` / `stage`, as the
    /// page spells them. The screen consumes it and clears it.
    @Published var pickRequest: String?
    /// `Open run ›` was pressed, by ATTEMPT. Consumed and cleared like the
    /// pick.
    @Published var openRunRequest: Int64?
    @Published private(set) var atBottom = true
    @Published private(set) var atTop = true

    @Published var filePreview: FilePreview?
    @Published var fileShare: FilePreview?
    @Published var viewedImage: ViewedImage?
    @Published var videoPlayback: VideoPlayback?

    private let client: any BayboClientProtocol
    private let supportDirectory: URL
    private let commentOutbox: IssueCommentOutbox
    /// A persisted `sending` row is resumed once when this card comes back.
    /// The server-side client id makes that replay safe across process death.
    private var resumedPersistedComments = false
    private var activeCommentMsgIds: Set<String> = []
    private let pasteboard: any PasteboardReading
    private weak var bridge: IssueBridge?
    private(set) var pageState: PageState?
    private(set) var composerTop: CGFloat?
    private(set) var bottomInset = 0
    /// A mirror may paint, but must not arm live controls or advance read state.
    @Published private(set) var isFromMirror = false
    private var issueIsLive = false
    private var timelineIsLive = false
    private var runsAreLive = false
    /// Only the newest refresh may commit. Actor reentrancy lets a second
    /// invalidation finish while the first refresh is suspended in I/O.
    private var refreshGeneration: UInt64 = 0
    /// Locally answered historical call ids stay retired until a live refresh
    /// proves what is still answerable.
    @Published private var retiredApprovalCallIds: Set<String> = []
    /// The card is stamped read once its timeline has rendered, and once —
    /// re-stamping per delivery would spend a round trip per comment.
    private var stampedRead = false
    private var invalidations: ProjectInvalidations.Token?

    private var composerDraft: ComposerStaging?
    var staging: ComposerStaging {
        if let composerDraft { return composerDraft }
        let made = ComposerStaging(
            host: self, client: client, pasteboard: pasteboard,
            supportDirectory: supportDirectory)
        composerDraft = made
        return made
    }

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
        supportDirectory: URL = SessionIndex.supportDirectory(),
        pasteboard: any PasteboardReading = Pasteboards.launch(),
        seed: Seed? = nil
    ) {
        self.projectId = projectId
        self.number = number
        self.client = client
        self.supportDirectory = supportDirectory
        self.pasteboard = pasteboard
        commentOutbox = IssueCommentOutbox(
            projectId: projectId, number: number, supportDirectory: supportDirectory)
        #if DEBUG
            if seedDemoCard() { return }
        #endif
        if !loadMirror(), let seed {
            issue = seed.issue
            runs = seed.runs
            team = seed.team
            children = seed.children
            isFromMirror = true
        }
    }

    deinit {
        MainActor.assumeIsolated { leaveCard() }
    }

    /// Attach whichever warm renderer currently leases this visit.
    func attach(_ bridge: IssueBridge) {
        if let current = self.bridge, current !== bridge {
            media.detach(current)
        }
        self.bridge = bridge
        media.attach(bridge)
        guard invalidations == nil else { return }
        invalidations = ProjectInvalidations.shared.observe { [weak self] change in
            guard let self else { return }
            guard change.scope == "stale" || change.projectId == self.projectId else { return }
            guard change.issueNumber == nil || change.issueNumber == self.number else { return }
            self.invalidated()
        }
    }

    func detach(_ bridge: IssueBridge) {
        guard self.bridge === bridge else { return }
        media.detach(bridge)
        self.bridge = nil
    }

    // MARK: - Reads

    func refresh() async {
        #if DEBUG
            if isDemoCard {
                _ = seedDemoCard()
                deliver()
                return
            }
        #endif
        refreshGeneration &+= 1
        let generation = refreshGeneration
        isRefreshing = true
        defer {
            if refreshGeneration == generation { isRefreshing = false }
        }
        async let issue = try? client.projectIssueGet(projectId: projectId, number: number)
        async let eventsJson = try? client.projectIssueEvents(
            projectId: projectId, number: number)
        async let runs = try? client.projectIssueRuns(projectId: projectId, number: number)
        async let siblings = try? client.projectIssues(projectId: projectId)

        let fetchedEventsJson = await eventsJson
        guard refreshGeneration == generation else { return }
        if let json = fetchedEventsJson {
            self.eventsJson = json
            let timeline =
                (try? IssueEvent.decodeTimeline(json)) ?? (events: [], firstUnread: nil)
            events = timeline.events
            firstUnread = timeline.firstUnread
            timelineIsLive = true
            reconcileConfirmedComments()
        }
        persistMirror()
        deliver()
        resumePersistedComments()

        let fetchedIssue = await issue
        guard refreshGeneration == generation else { return }
        if let fetchedIssue {
            self.issue = fetchedIssue
            issueIsLive = true
            isFromMirror = false
        }
        persistMirror()
        deliver()

        let (fetchedRuns, fetchedSiblings) = await (runs, siblings)
        guard refreshGeneration == generation else { return }
        // The log carries totals beside the rows; the page prints the rows and
        // the totals belong to a screen that does not exist yet (P7).
        if let fetchedRuns {
            self.runs = fetchedRuns.runs
            runsAreLive = true
        }
        // Children come from the board: the card DTO carries a done/total
        // count and no list at all.
        if let fetchedSiblings {
            children = fetchedSiblings.filter { $0.parent == number }
        }
        persistMirror()
        deliver()

        guard team.isEmpty,
            let fetchedTeam = try? await client.projectTeam(projectId: projectId)
        else { return }
        guard refreshGeneration == generation else { return }
        team = fetchedTeam
        persistMirror()
        deliver()
    }

    // MARK: - Mirror
    // Mirrors are wholesale snapshots; merging would preserve stale server fields.

    private var mirrorURL: URL? {
        Self.mirrorURL(projectId: projectId, number: number, in: supportDirectory)
    }

    private static func mirrorURL(projectId: String, number: Int64, in directory: URL) -> URL? {
        // The project id reaches the filesystem, so it may not name a path;
        // the number is an Int64 and cannot.
        guard !projectId.isEmpty, !projectId.contains("/"), !projectId.contains(".") else {
            return nil
        }
        return directory.appendingPathComponent("issue-\(projectId)-\(number).json")
    }

    static func discardMirror(
        projectId: String, number: Int64,
        supportDirectory: URL = SessionIndex.supportDirectory()
    ) {
        guard let url = mirrorURL(projectId: projectId, number: number, in: supportDirectory)
        else { return }
        try? FileManager.default.removeItem(at: url)
    }

    @discardableResult
    private func loadMirror() -> Bool {
        guard let url = mirrorURL, let data = try? Data(contentsOf: url),
            let mirror = try? JSONDecoder().decode(
                ProjectsStore.IssueContentMirror.self, from: data)
        else { return false }
        issue = mirror.issue.info
        eventsJson = mirror.eventsJson
        // The entries, and deliberately not the boundary they were written
        // with — see `firstUnread`.
        events = (try? IssueEvent.decodeList(mirror.eventsJson)) ?? []
        runs = mirror.runs.map(\.info)
        team = mirror.team.map(\.info)
        children = mirror.children.map(\.info)
        isFromMirror = true
        return true
    }

    private func persistMirror() {
        #if DEBUG
            // The demo is memory-only, `ProjectsStore`'s rule: a fixture left
            // on disk is a card a later plain launch would find and believe.
            if isDemoCard { return }
        #endif
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

    func resync() {
        // Drop disk, memory, and web state together so an unknown page state
        // cannot survive a protocol reset through another cache.
        if let url = mirrorURL {
            try? FileManager.default.removeItem(at: url)
        }
        issue = nil
        events = []
        eventsJson = "{\"items\":[]}"
        firstUnread = nil
        runs = []
        children = []
        writeError = nil
        pageState = nil
        // Nothing is from a mirror any more, because nothing is here at all.
        isFromMirror = false
        issueIsLive = false
        timelineIsLive = false
        runsAreLive = false
        refreshGeneration &+= 1
        stampedRead = false
        bridge?.rebuild()
        Task { await refresh() }
    }

    private func leaveCard() {
        // Upload tasks may outlive a screen; retire only when the visit deinitializes.
        composerDraft?.retire()
        composerDraft = nil
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

    /// Enrol and paint a comment before touching the network. The returned id
    /// is both the optimistic row's identity and the server idempotency key.
    @discardableResult
    func sendComment(
        _ text: String,
        attachments: [AttachmentRef] = [],
        unblockAfterSend: Bool = false
    ) -> String {
        writeError = nil
        let clientMsgId = UUID().uuidString.lowercased()
        commentOutbox.begin(
            clientMsgId: clientMsgId,
            text: text.trimmingCharacters(in: .whitespacesAndNewlines),
            attachments: attachments,
            unblockAfterSend: unblockAfterSend)
        deliver()
        dispatchComment(clientMsgId)
        return clientMsgId
    }

    /// Retry the failed row in place. The webview sends only its id; the
    /// persisted outbox remains the authority for text and attachments.
    func retryComment(_ clientMsgId: String) {
        guard commentOutbox.entry(clientMsgId)?.state == .failed else { return }
        writeError = nil
        commentOutbox.resetForRetry(clientMsgId)
        deliver()
        dispatchComment(clientMsgId)
    }

    var pendingComments: [PendingIssueComment] {
        commentOutbox.entries()
    }

    private func dispatchComment(_ clientMsgId: String) {
        guard activeCommentMsgIds.insert(clientMsgId).inserted else { return }
        #if DEBUG
            if isDemoCard {
                commentOutbox.markFailed(clientMsgId)
                activeCommentMsgIds.remove(clientMsgId)
                writeError = "The demo card has no gateway."
                deliver()
                return
            }
        #endif
        Task { [weak self] in
            await self?.postComment(clientMsgId)
        }
    }

    private func postComment(_ clientMsgId: String) async {
        defer { activeCommentMsgIds.remove(clientMsgId) }
        guard let pending = commentOutbox.entry(clientMsgId) else { return }
        do {
            let json = try await client.projectIssueComment(
                projectId: projectId,
                number: number,
                clientMsgId: clientMsgId,
                text: pending.text,
                attachments: pending.attachments.map(\.request))
            mergeCommentEntry(json, clientMsgId: clientMsgId)
            let confirmed = commentOutbox.confirm(clientMsgId)
            writeError = nil
            deliver()
            // The comment must be durable before unblock rebuilds the agent
            // brief from the timeline.
            if confirmed?.unblockAfterSend == true { unblock() }
            // The response gives us the exact row immediately; the broader
            // refresh follows in the background for updated card/run state.
            await refresh()
        } catch {
            commentOutbox.markFailed(clientMsgId)
            writeError = ProjectsStore.message(from: error)
            deliver()
        }
    }

    private func reconcileConfirmedComments() {
        for clientMsgId in Set(events.compactMap(\.clientMsgId)) {
            guard let confirmed = commentOutbox.confirm(clientMsgId) else { continue }
            if confirmed.unblockAfterSend { unblock() }
        }
    }

    private func resumePersistedComments() {
        guard !resumedPersistedComments else { return }
        resumedPersistedComments = true
        for pending in commentOutbox.entries() where pending.state == .sending {
            dispatchComment(pending.clientMsgId)
        }
    }

    private func mergeCommentEntry(_ json: String, clientMsgId: String) {
        guard let data = json.data(using: .utf8),
            var item = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let id = item["id"] as? String
        else { return }
        item["client_msg_id"] = clientMsgId

        var envelope: [String: Any] = [:]
        if let current = eventsJson.data(using: .utf8),
            let decoded = try? JSONSerialization.jsonObject(with: current) as? [String: Any]
        {
            envelope = decoded
        }
        var items = envelope["items"] as? [[String: Any]] ?? []
        if let index = items.firstIndex(where: { candidate in
            candidate["id"] as? String == id
                || candidate["client_msg_id"] as? String == clientMsgId
        }) {
            items[index] = item
        } else {
            items.append(item)
        }
        envelope["items"] = items
        guard let merged = try? JSONSerialization.data(withJSONObject: envelope),
            let encoded = String(data: merged, encoding: .utf8)
        else { return }
        eventsJson = encoded
        events = (try? IssueEvent.decodeList(encoded)) ?? events
        persistMirror()
    }

    func resolveApproval(callId: String, decision: IssueApprovalDecision) {
        retiredApprovalCallIds.insert(callId)
        writeError = nil
        Task {
            do {
                try await client.projectIssueApprovalResolve(
                    projectId: projectId, number: number, callId: callId, decision: decision)
                await refresh()
            } catch {
                let message = ProjectsStore.message(from: error)
                if ProjectsStore.readsAsGone(message) {
                    await refresh()
                } else {
                    retiredApprovalCallIds.remove(callId)
                    writeError = message
                }
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

    func unblock() {
        Task {
            await write { [projectId, number] client in
                _ = try await client.projectIssuePatch(
                    projectId: projectId, number: number,
                    patch: IssuePatch(
                        title: nil, description: nil, attachments: nil, priority: nil,
                        assignee: .keep, blockedReason: .clear, cancelled: nil, parent: nil,
                        detachParent: false, stage: nil, pinned: nil))
            }
        }
    }

    func storeGeneratedFace(agentId: String, pngBase64: String) {
        #if DEBUG
            if isDemoCard { return }
        #endif
        guard let data = Data(base64Encoded: pngBase64) else { return }
        // Cosmetic and fire-and-forget: upload before compare-and-set so another
        // device cannot observe a dangling avatar reference.
        Task { [client, projectId] in
            do {
                let blobId = try await AgentFaceUpload.put(data, client: client)
                try await client.agentSetAvatarIfEmpty(agentId: agentId, blobId: blobId)
            } catch {
                NSLog("baybo: generated face for %@: %@", agentId, bayboErrorText(error))
                return
            }
            // The roster is what every face on both surfaces is drawn from,
            // so the board refetches too — not just this card.
            await refresh()
            AppStore.shared?.projectsStore.scheduleBoardRefresh(projectId)
        }
    }

    func clearWriteError() { writeError = nil }

    func showWriteError(_ message: String?) {
        writeError = message
    }

    func setAtBottom(_ value: Bool) {
        atBottom = value
    }

    func setAtTop(_ value: Bool) {
        atTop = value
    }

    func rememberPageState(scrollTop: Double, folds: [String: Bool]) {
        guard scrollTop.isFinite, scrollTop >= 0 else { return }
        pageState = PageState(scrollTop: scrollTop, folds: folds)
    }

    func setComposerTop(_ value: CGFloat) {
        composerTop = value
        bridge?.setComposerTop(value)
    }

    func rememberBottomInset(_ value: Int) {
        bottomInset = value
    }

    func jumpToLatest() {
        bridge?.jumpToLatest()
    }

    func scrollToTop() {
        bridge?.scrollToTop()
    }

    func markRendered() {
        #if DEBUG
            if isDemoCard { return }
        #endif
        guard timelineIsLive, !stampedRead else { return }
        stampedRead = true
        Task { [projectId, number] in
            do {
                try await client.projectIssueRead(projectId: projectId, number: number)
            } catch {
                stampedRead = false
                NSLog("baybo: mark read #%lld: %@", number, bayboErrorText(error))
                return
            }
            if let issue {
                self.issue = issue.with(unread: 0)
                persistMirror()
                deliver()
            }
            AppStore.shared?.projectsStore.noteIssueRead(board: projectId, issue: number)
            AppStore.shared?.projectsStore.scheduleBoardRefresh(projectId)
            AppStore.shared?.projectsStore.scheduleRootRefresh()
        }
    }

    // MARK: - Derived

    var liveRun: IssueRunInfo? {
        // Mirror rows may render history; only a live response may drive controls.
        guard runsAreLive else { return nil }
        return runs.first { $0.settledAtMs == nil }
    }

    var pendingApprovals: [IssueApprovalPrompt] {
        // Timeline replay is historical; the live card bit authorizes the action.
        guard issueIsLive, timelineIsLive, issue?.approvalPending == true else { return [] }
        return IssueTimeline.pendingApprovals(in: events).filter {
            !retiredApprovalCallIds.contains($0.callId)
        }
    }

    func handle(forAgent agentId: String) -> String {
        AgentHandles.handle(forAgent: agentId, in: team)
    }

    // MARK: - The page

    private func deliver() {
        guard let issue else { return }
        bridge?.deliver(
            issue: issue, eventsJson: eventsJson, runs: runs, people: people,
            children: children, firstUnread: firstUnread, timelineLive: timelineIsLive,
            pendingComments: pendingComments)
    }

    var people: [String: IssuePerson] {
        let monograms = AgentMonogram.map(for: team)
        return Dictionary(
            team.map { member in
                (
                    member.id,
                    IssuePerson(
                        handle: member.handle,
                        avatar: member.avatarBlobId,
                        monogram: monograms[member.id] ?? AgentMonogram.of(member.handle))
                )
            }, uniquingKeysWith: { a, _ in a })
    }

    /// Re-send whatever is loaded. Called when the page reports `ready` after
    /// a crash reload — its React tree is new and holds nothing.
    func redeliver() { deliver() }

    // MARK: - Attachments

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

}

extension IssueStore: ComposerHost {
    var draftKey: DraftKey { .card(project: projectId, number: number) }
}

#if DEBUG

    extension IssueStore {
        static let demoCardArg = "-baybo-demo-card"

        private var isDemoCard: Bool { ProjectsStore.demoRequested }

        private func seedDemoCard() -> Bool {
            guard isDemoCard,
                let board = AppStore.shared?.projectsStore.boards[projectId],
                let card = board.issues.first(where: { $0.number == number })
            else { return false }
            issue = card.with(
                description: Self.demoDescription,
                branch: .set(
                    value: "project-\(projectId)/feat/issue-\(number)-dial-loop-subscription"),
                blockedReason: .set(value: Self.demoBlockedReason))
            team = board.team
            runs = board.runs.contains { $0.number == number } ? Self.demoRuns(number: number) : []
            eventsJson = Self.demoEventsJson(number: number, assignee: card.assignee ?? "a-dev")
            events = (try? IssueEvent.decodeList(eventsJson)) ?? []
            children = board.issues.filter { $0.parent == number }
            isFromMirror = false
            issueIsLive = true
            timelineIsLive = true
            runsAreLive = true
            return true
        }

        private static let demoBlockedReason =
            "needs a decision on transport::supervisor::redial_until_connected_after_the_second_leg"

        private static let demoDescription = """
            The dial loop stops resubscribing after the second redial: the leg \
            comes back, the session never re-attaches, and the card sits \
            **WORKING** with nothing arriving.

            `transport::supervisor::redial_until_connected` drops the \
            subscription set on the way out.
            """

        private static func demoRuns(number: Int64) -> [IssueRunInfo] {
            let now = Int64(Date().timeIntervalSince1970 * 1000)
            return [
                IssueRunInfo(
                    number: number, attempt: 3, agentId: "a-dev", status: .running,
                    trigger: .promoted, sessionId: "s-\(number)", error: nil,
                    createdAtMs: now - 780_000, startedAtMs: now - 720_000, settledAtMs: nil,
                    costMicros: nil, inputTokens: nil, outputTokens: nil),
                IssueRunInfo(
                    number: number, attempt: 2, agentId: "a-dev2", status: .failed,
                    trigger: .retry, sessionId: "s-\(number)-2",
                    error: "the sandbox exited 137", createdAtMs: now - 7_200_000,
                    startedAtMs: now - 7_100_000, settledAtMs: now - 6_900_000,
                    costMicros: 41_000, inputTokens: 12_000, outputTokens: 900),
                // An attempt that never got a slot: a row somebody still wants
                // to SEE, with no session and therefore no transcript to open.
                IssueRunInfo(
                    number: number, attempt: 1, agentId: "a-lead", status: .cancelled,
                    trigger: .triage, sessionId: nil, error: nil,
                    createdAtMs: now - 86_400_000, startedAtMs: nil,
                    settledAtMs: now - 86_300_000, costMicros: nil, inputTokens: nil,
                    outputTokens: nil),
            ]
        }

        private static func demoEventsJson(number: Int64, assignee: String) -> String {
            let now = Int64(Date().timeIntervalSince1970 * 1000)
            let items: [[String: Any]] = [
                [
                    "id": "ev-1", "number": number, "actor": ["kind": "user"],
                    "body": ["kind": "opened"], "created_at_ms": now - 86_400_000,
                ],
                [
                    "id": "ev-2", "number": number, "actor": ["kind": "system"],
                    "body": ["kind": "moved", "from": "todo", "to": "in_progress"],
                    "created_at_ms": now - 80_000_000,
                ],
                [
                    "id": "ev-3", "number": number,
                    "actor": ["kind": "agent", "id": assignee, "handle": "dev-1"],
                    "body": [
                        "kind": "comment",
                        "text": "Reproduced on the second redial. The subscription set is "
                            + "rebuilt from `pending`, which the dial path has already "
                            + "drained.",
                    ],
                    "created_at_ms": now - 7_200_000,
                ],
                [
                    "id": "ev-4", "number": number, "actor": ["kind": "system"],
                    "body": ["kind": "run_settled", "attempt": 2, "status": "failed"],
                    "created_at_ms": now - 6_900_000,
                ],
                [
                    "id": "ev-5", "number": number, "actor": ["kind": "system"],
                    "body": ["kind": "run_started", "attempt": 3],
                    "created_at_ms": now - 6_800_000,
                ],
                [
                    "id": "ev-6", "number": number, "actor": ["kind": "user"],
                    "body": ["kind": "comment", "text": "Try it again once the fence lands."],
                    "created_at_ms": now - 1_800_000,
                ],
            ]
            let envelope: [String: Any] = ["items": items, "first_unread": "ev-6"]
            guard let data = try? JSONSerialization.data(withJSONObject: envelope),
                let json = String(data: data, encoding: .utf8)
            else { return "{\"items\":[]}" }
            return json
        }
    }

#endif
