import Foundation

/// One project card, as the card page renders it.
///
/// A separate type from `ProjectsStore` rather than a slice of it, and for the
/// reason the board's own writes are in the store: this holds one card visit's
/// state — a weak renderer bridge, page position, bottom inset and live
/// approval queue — none of which a board screen has any business carrying,
/// and all of which would need a per-card key inside a store keyed by board.
///
/// Attachments are not reimplemented: `TranscriptMedia` is the same engine the
/// chat transcript uses, so a file card on a project card behaves exactly as
/// it does in a conversation.
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
    /// The timeline's raw `{"items":[…]}` envelope, kept alongside the
    /// decoded form. The page renders the gateway's own shape, so it gets the
    /// bytes verbatim — re-encoding through a Swift mirror would be a third
    /// place every new event kind has to be taught about.
    private var eventsJson = "{\"items\":[]}"
    /// Where the operator stopped reading: the oldest entry they have not
    /// seen, resolved by the gateway (`IssueTimelineDto.first_unread`).
    ///
    /// **Live only**, and never restored from the mirror even though the
    /// envelope on disk still carries it — the same rule `liveRun` and
    /// `pendingApprovals` are under, for the same reason. This card was read
    /// the moment it was last opened, so a boundary replayed off disk points
    /// at a line the operator has already crossed, and the page would open
    /// halfway up a thread with a rule promising news that is not there.
    private(set) var firstUnread: String?
    @Published private(set) var runs: [IssueRunInfo] = []
    @Published private(set) var team: [TeamMemberInfo] = []
    @Published private(set) var children: [IssueInfo] = []
    /// The last write's failure, in the server's own words. Drawn as a banner
    /// at the top of the screen, and cleared on entry by every write.
    @Published private(set) var writeError: String?
    /// The dock's own line — what the attachment strip has to say (too large,
    /// still uploading, that file could not be read).
    ///
    /// Deliberately NOT `writeError`: every `write` clears that on entry, so
    /// resolving an approval would wipe "attachment too large", and it renders
    /// at the top of the page rather than under the strip that raised it. The
    /// strip's retraction discipline assumes a slot only it writes.
    @Published var notice: String?
    @Published private(set) var isRefreshing = false

    // MARK: - What the page asks the screen for
    //
    // Three signals the card page raises and only a screen can answer: open a
    // run's transcript, raise a picker, and whether the reader is parked at the
    // newest activity. They land HERE, as state, rather than on closures the
    // screen installs on `IssueBridge` — which is what they were, and what
    // quietly made the screen immortal.
    //
    // A closure written inside a `View`'s body captures that whole struct,
    // property wrappers included. A pool host retaining such a closure would
    // retain every visit it had rendered: its invalidation observer would
    // refetch forever and its staging machine would never retire.
    // `IssueBridge.store` is WEAK, so routing through the store cannot cycle no
    // matter what the screen does — and with the closures gone there is nowhere
    // to install one.

    /// A chip was pressed: `status` / `priority` / `assignee` / `stage`, as the
    /// page spells them. The screen consumes it and clears it.
    @Published var pickRequest: String?
    /// `Open run ›` was pressed, by ATTEMPT. Consumed and cleared like the
    /// pick.
    @Published var openRunRequest: Int64?
    /// Whether the page is parked at its newest activity — the page reports it
    /// on every scroll and once per delivery, and it drives the way back down.
    ///
    /// Starts true so a card that fits its screen never flashes a disc on the
    /// way in.
    @Published private(set) var atBottom = true

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
    /// Prevent a refresh and a button tap from dispatching the same local row
    /// concurrently. The durable key protects the server; this keeps the
    /// client from spending two requests needlessly.
    private var activeCommentMsgIds: Set<String> = []
    /// The system clipboard, behind the paste row. Injected for the reason
    /// `ChatStore`'s is: `UIPasteboard.general` is process-global and
    /// swift-testing runs suites in PARALLEL, so one suite's paste written to
    /// the real board surfaces as another's logic bug.
    private let pasteboard: any PasteboardReading
    private weak var bridge: IssueBridge?
    private(set) var pageState: PageState?
    private(set) var composerTop: CGFloat?
    private(set) var bottomInset = 0
    /// Everything on screen came off DISK and no live fetch has landed yet.
    ///
    /// Load-bearing, not cosmetic: a mirrored card may name a prompt that
    /// timed out hours ago and a run that has long since settled, and both of
    /// those drive controls that ACT. Content paints from the mirror; anything
    /// that presses a live queue waits for the network.
    @Published private(set) var isFromMirror = false
    /// Freshness is tracked for each response that can arm a control. A live
    /// card response says nothing about whether the independently fetched run
    /// log or timeline is live too.
    private var issueIsLive = false
    private var timelineIsLive = false
    private var runsAreLive = false
    /// Only the newest refresh may commit. Actor reentrancy lets a second
    /// invalidation finish while the first refresh is suspended in I/O.
    private var refreshGeneration: UInt64 = 0
    /// A historical approval row outlives its live gateway call. Once the
    /// gateway says a call is gone, keep it out of the actionable queue so a
    /// newer live prompt behind it remains reachable.
    @Published private var retiredApprovalCallIds: Set<String> = []
    /// The card is stamped read once its timeline has rendered, and once —
    /// re-stamping per delivery would spend a round trip per comment.
    private var stampedRead = false
    private var invalidations: ProjectInvalidations.Token?

    /// The card's composer draft: what has been typed and what is staged.
    ///
    /// An explicit optional built on first use rather than a `lazy var`, which
    /// is `ChatStore`'s rule and for its reason: constructing one reads the
    /// draft off disk and RESUMES the uploads it still owes, so a store built
    /// to prefetch a card — or an SwiftUI preview — would start uploads nobody
    /// asked for.
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
        #if DEBUG
            // The demo card's fixture is the answer; asking the gateway would
            // be five 404s on a board that only exists in memory. Re-seeded
            // rather than skipped, because the board fixture is where a demo
            // write lands: picking a status or an assignee edits the in-memory
            // board (`ProjectsStore.write`), and this is what carries that
            // back onto the card. See `seedDemoCard`.
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
        async let team = try? client.projectTeam(projectId: projectId)
        async let siblings = try? client.projectIssues(projectId: projectId)

        let fetched = await (issue, eventsJson, runs, team, siblings)
        guard refreshGeneration == generation else { return }
        let (fetchedIssue, fetchedEventsJson, fetchedRuns, fetchedTeam, fetchedSiblings) = fetched
        if let fetchedIssue { self.issue = fetchedIssue }
        if let json = fetchedEventsJson {
            self.eventsJson = json
            let timeline =
                (try? IssueEvent.decodeTimeline(json)) ?? (events: [], firstUnread: nil)
            events = timeline.events
            firstUnread = timeline.firstUnread
            timelineIsLive = true
            reconcileConfirmedComments()
        }
        // The log carries totals beside the rows; the page prints the rows and
        // the totals belong to a screen that does not exist yet (P7).
        if let fetchedRuns {
            self.runs = fetchedRuns.runs
            runsAreLive = true
        }
        if let fetchedTeam { self.team = fetchedTeam }
        // Children come from the board: the card DTO carries a done/total
        // count and no list at all.
        if let fetchedSiblings {
            children = fetchedSiblings.filter { $0.parent == number }
        }
        // THIS fetch's own answer, not `self.issue` — that is non-nil the
        // moment a mirror loads, so reading it would arm the live controls off
        // a cached card the network never confirmed.
        if fetchedIssue != nil {
            issueIsLive = true
            isFromMirror = false
        }
        persistMirror()
        deliver()
        resumePersistedComments()
    }

    // MARK: - Mirror
    //
    // REPLACE, never merge — the board's rule, for the board's reason: there
    // is no local state here worth protecting, and a merge would only invent
    // ways for the two to disagree.

    private var mirrorURL: URL? {
        Self.mirrorURL(projectId: projectId, number: number, in: supportDirectory)
    }

    /// Where a card's local copy lives. Static because the BOARD deletes one
    /// for a card it has not opened — see `discardMirror` — and a second
    /// spelling of this path is a rebuild that silently rebuilds nothing.
    private static func mirrorURL(projectId: String, number: Int64, in directory: URL) -> URL? {
        // The project id reaches the filesystem, so it may not name a path;
        // the number is an Int64 and cannot.
        guard !projectId.isEmpty, !projectId.contains("/"), !projectId.contains(".") else {
            return nil
        }
        return directory.appendingPathComponent("issue-\(projectId)-\(number).json")
    }

    /// Throw a card's local copy away WITHOUT opening it — the board row's
    /// "Rebuild this card".
    ///
    /// The same first step as `resync`, and the only one that means anything
    /// from a list: there is no page to reload and no memory to clear, so the
    /// next open starts without card content by construction. Deliberately not
    /// a store method: making one for a card nobody is looking at would install
    /// observers and card state merely to delete a file.
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

    /// This screen is going away for good. The staging machine outlives the
    /// frame — an upload holds it — so a re-push would build a SECOND one over
    /// the same draft key, and the zombie's terminal write would put a sent
    /// draft straight back on disk.
    ///
    /// **"For good" is what `deinit` means and what `.onDisappear` does not.**
    /// It hung off the latter, where a push covering the card — a sub-issue,
    /// the `↳ #N` parent chip — retired the staging mid-visit: every in-flight
    /// upload cancelled, the staged strip dropped, and the next read of
    /// `staging` lazily building exactly the second machine this exists to
    /// prevent.
    private func leaveCard() {
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
            // Demo boards have no gateway by construction. Keep the full
            // optimistic/failure UI testable without touching whichever real
            // gateway the simulator may otherwise be paired with.
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
            // The comment is durable before this runs. The unblock rebuilds
            // its brief from the card, so reversing the order loses the answer
            // the agent stopped to ask for.
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

    /// A timeline fetch is also a durability receipt. This closes both races:
    /// the invalidation can beat the POST response, and a process can die
    /// after the server writes but before the response reaches the phone.
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

    /// Fold the POST's exact event into the raw timeline immediately. This is
    /// the path the FFI always documented but the app previously ignored in
    /// favour of five unrelated refetches.
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
                        detachParent: false, stage: nil, pinned: nil))
            }
        }
    }

    /// Store the face the page drew for an agent that had none.
    ///
    /// **Fire and forget, and silent on failure.** Nobody asked for this: it
    /// is the page noticing a teammate has no picture and offering one, so a
    /// refusal (offline, a gateway that 400s the blob, an agent removed since
    /// the card was fetched) must cost the operator nothing — the agent keeps
    /// its monogram, which is what it had a moment ago. `writeError` is for
    /// what the operator asked for, and putting this in it would raise a
    /// banner over a card nobody touched.
    ///
    /// Two calls in this order: the bytes become a blob, then the agent
    /// points at it. The gateway stats the blob on the way in, so the reverse
    /// order is a refusal.
    func storeGeneratedFace(agentId: String, pngBase64: String) {
        #if DEBUG
            // A blob upload followed by an avatar PATCH — for six agents that
            // do not exist, on the operator's real gateway, once per open. The
            // demo seeds a team where the board's fixture leaves most faces
            // empty, and the page draws one for each; nothing before this
            // change ever delivered a payload here, so the generator had
            // nothing to fire on. `ProjectsStore`'s rule, applied: the demo
            // touches neither the network nor the disk.
            if isDemoCard { return }
        #endif
        guard let data = Data(base64Encoded: pngBase64) else { return }
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

    /// Carry a refusal raised somewhere else onto this screen's banner.
    ///
    /// The chips write through `ProjectsStore` — a move sends the destination
    /// column's whole order, and that rule has one home — so their refusals
    /// land on THAT store's `writeError` while the banner over this page reads
    /// this one. Verbatim, like every other refusal here: the gateway's
    /// sentences name which ceiling, which block, which card holds the slot.
    func showWriteError(_ message: String?) {
        writeError = message
    }

    func setAtBottom(_ value: Bool) {
        atBottom = value
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

    func setLanguage(_ code: String) {
        bridge?.setLanguage(code)
    }

    func jumpToLatest() {
        bridge?.jumpToLatest()
    }

    /// Stamp the card read. Called from the page's own "I rendered" message —
    /// never on the way in, because a card whose timeline threw has not been
    /// read by anybody.
    func markRendered() {
        #if DEBUG
            if isDemoCard { return }
        #endif
        guard timelineIsLive, !stampedRead else { return }
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
        guard runsAreLive else { return nil }
        return runs.first { $0.settledAtMs == nil }
    }

    /// Prompts parked on this card, **once a live answer has landed**.
    ///
    /// Never from the mirror, for the reason the board refuses to cache them
    /// at all: a prompt is a live queue entry with a 300s timeout, and one
    /// replayed off disk would offer an answer to something that stopped
    /// listening hours ago.
    var pendingApprovals: [IssueApprovalPrompt] {
        guard issueIsLive, timelineIsLive, issue?.approvalPending == true else { return [] }
        return IssueTimeline.pendingApprovals(in: events).filter {
            !retiredApprovalCallIds.contains($0.callId)
        }
    }

    func handle(forAgent agentId: String) -> String {
        AgentHandles.handle(forAgent: agentId, in: team)
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
            issue: issue, eventsJson: eventsJson, runs: runs, people: people,
            children: children, firstUnread: firstUnread, timelineLive: timelineIsLive,
            pendingComments: pendingComments)
    }

    /// Who the ids on this card's DTOs are: what to call them, and what to
    /// draw for them.
    ///
    /// The monogram is resolved HERE rather than on the page, because it is a
    /// property of the whole team and not of one handle — `dev-1` and `docs-1`
    /// both give `D1` until the set widens (see `AgentMonogram`). A page
    /// deriving its own from the handle it was handed would print exactly the
    /// collision the board already knows how to avoid.
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

}

/// A card is a composer host: it holds the dock's notice line and names the
/// draft. Its key lives under `card-drafts/`, never beside the conversations —
/// see `DraftScope`.
extension IssueStore: ComposerHost {
    var draftKey: DraftKey { .card(project: projectId, number: number) }
}

#if DEBUG

    /// `-baybo-demo-card`: the card page with something in every part of it.
    ///
    /// It used to open on its own loading line for ever — the store talks to a
    /// gateway and the demo has none — so the ONLY thing reachable headlessly
    /// was the shell around an empty page. Everything the card itself draws
    /// (the head, the description, the state band, the sub-issues, the
    /// Activity) and everything the ⋯ offers about a run had no test tier at
    /// all.
    ///
    /// The card, the team and the number come from the BOARD fixture rather
    /// than from a second copy here: `-baybo-demo-card` lands on card #41 of
    /// the demo board, and two fixtures for one card is two things to keep in
    /// step. What this adds is what the board's own rows leave empty — the
    /// description, the branch, the run LOG (a board holds only unsettled
    /// runs) and a timeline.
    extension IssueStore {
        static let demoCardArg = "-baybo-demo-card"

        /// This card belongs to a demo board: the fixture is the answer, and
        /// the network must not be touched.
        ///
        /// NOT keyed on `-baybo-demo-card` — that flag only says which screen
        /// to LAND on, and a card reached by tapping a row is the same card.
        /// Keying the seed to the landing flag gave one card two different
        /// pages depending on how you got to it.
        private var isDemoCard: Bool { ProjectsStore.demoRequested }

        /// Fill this store from the demo board. Answers whether it did, so the
        /// caller can fall through to the real cold-open path when the flag is
        /// absent — or when the board fixture has no such card.
        ///
        /// Seeding is only half of it: a store with a card in it DELIVERS, and
        /// delivering is what arms the page's own side effects. `refresh`,
        /// `markRendered` and `storeGeneratedFace` are all gated on
        /// `isDemoCard` for that reason — the last one would otherwise upload a
        /// generated face and PATCH an avatar for every faceless agent in the
        /// fixture, against whatever gateway the simulator happens to be
        /// paired with, once per open.
        private func seedDemoCard() -> Bool {
            guard isDemoCard,
                let board = AppStore.shared?.projectsStore.boards[projectId],
                let card = board.issues.first(where: { $0.number == number })
            else { return false }
            // The board's own row, plus the three fields a board row leaves
            // empty. Through `with` rather than the full initialiser, which is
            // what that helper exists for: a field added to the record
            // upstream must fail ONE file to compile, not be silently dropped
            // by whichever call site nobody updated.
            issue = card.with(
                description: Self.demoDescription,
                branch: .set(
                    value: "project-\(projectId)/feat/issue-\(number)-dial-loop-subscription"),
                blockedReason: .set(value: Self.demoBlockedReason))
            team = board.team
            // A LOG only for a card the board says has run. The board fixture
            // carries the live rows (41, 42, 43) and nothing else, and a seed
            // that invented three attempts for every card would leave the demo
            // with no card that has never run — which is a state the card page
            // draws differently (its ⋯ has nothing in it and is not drawn).
            runs = board.runs.contains { $0.number == number } ? Self.demoRuns(number: number) : []
            eventsJson = Self.demoEventsJson(number: number, assignee: card.assignee ?? "a-dev")
            events = (try? IssueEvent.decodeList(eventsJson)) ?? []
            children = board.issues.filter { $0.parent == number }
            // NOT from a mirror: the fixture is what a live answer would have
            // said, so the controls a mirror withholds — Stop, the run rows —
            // are armed and therefore paintable.
            isFromMirror = false
            issueIsLive = true
            timelineIsLive = true
            runsAreLive = true
            return true
        }

        /// Why the card is stopped — and the page's one PAN VECTOR, which is
        /// what makes it worth putting in a fixture.
        ///
        /// The description cannot pan the page (`.md` clips its own overflow),
        /// so the only text that can is the text outside it, and the blocked
        /// note is where that arrives in real life: an agent naming the symbol
        /// it is stuck on. Long enough to beat the note's text column, which is
        /// narrower than the page by the label beside it.
        private static let demoBlockedReason =
            "needs a decision on transport::supervisor::redial_until_connected_after_the_second_leg"

        /// Markdown, and deliberately awkward markdown: a long unbroken
        /// identifier is what used to run past `.md`'s clip and pan the whole
        /// page sideways, so the fixture that a screenshot test looks at is
        /// the one that would show it coming back.
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
                // Consecutive machinery on purpose: one lone entry draws as a
                // line and two in a row collapse, so the fixture has to carry
                // both shapes or one of them is never looked at.
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
