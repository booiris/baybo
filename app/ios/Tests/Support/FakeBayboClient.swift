import Foundation

@testable import Baybo

/// A recording stand-in for the Rust core. UniFFI already generates
/// `BayboClientProtocol` (every method the app calls) and `BayboClient` conforms
/// to it, so `ChatStore` can be driven with no gateway, no tokio runtime and no
/// keychain — and every call it makes is inspectable.
///
/// The methods are `nonisolated` (the protocol is), so the recorded state is
/// behind a lock rather than an actor: `ChatStore` starts these calls from the
/// main actor but the async bodies run wherever.
final class FakeBayboClient: BayboClientProtocol, @unchecked Sendable {
    struct SendCall: Equatable {
        let sessionId: String
        let text: String
        let msgId: String
        let attachments: [AttachmentRef]
    }

    struct LookupCall: Equatable {
        let sessionId: String
        let platformMsgId: String
    }

    struct ApprovalCall: Equatable {
        let callId: String
        let decision: ApprovalDecision
    }

    struct DeckFileUpload: Equatable {
        let path: String
        let mimeType: String
        let cardId: String
    }

    struct SetModelCall: Equatable {
        let sessionId: String
        let llm: String?
        let model: String?
        let effort: String?
    }

    /// A composer upload (`ComposerStaging`). The path proves the bytes never
    /// crossed the FFI — every staged pick streams off a file.
    struct BlobUploadCall: Equatable {
        let path: String
        let mimeType: String
    }

    struct CommentCall {
        let number: Int64
        let clientMsgId: String
        let text: String
        let attachments: [IssueAttachmentInput]
    }

    /// Everything the app never calls under test. Distinct prose so an
    /// accidental reliance on it is obvious in the failure.
    private static let unsupported = BayboError.Other(
        message: "FakeBayboClient: unsupported call")

    private let lock = NSLock()

    private var sinks: [String: FrameSink] = [:]
    private var sends: [SendCall] = []
    private var sendAfterConnects: [SendCall] = []
    private var connects: [String] = []
    private var createdSessions: [String] = []
    private var lookups: [LookupCall] = []
    private var approvals: [ApprovalCall] = []
    private var marksRead: [Int64] = []
    private var batchMarksRead: [[String]] = []
    private var modelSets: [SetModelCall] = []
    private var modelReads: [String] = []
    /// Coarse cross-method call order ("create" / "setModel" / "send" /
    /// "sendAfterConnect") — the draft-pin contract is an ORDERING one (the
    /// pin must land between session creation and the first send), and the
    /// per-method arrays can't see across each other.
    private var callOrder: [String] = []

    private var deckFileUploads: [DeckFileUpload] = []
    private var deckFileUploadBlobId: String?

    private var blobUploads: [BlobUploadCall] = []
    private var blobUploadsHeld = false
    private var blobUploadWaiters: [CheckedContinuation<Void, Never>] = []
    private var blobUploadOutcome: Result<String, Error> = .success(FakeBayboClient.uploadedBlobId)
    private var blobDownloads: [String] = []
    private var blobCachedReads: [String] = []

    private var apiLegInvalidations = 0

    private var connectError: Error?
    private var sendError: Error?
    /// Fails ONLY `chatSend` (the connected fast path), leaving
    /// `chatSendAfterConnect` healthy — the stale-leg shape, where the registry
    /// disowns the current leg but a redial-and-send succeeds.
    private var plainSendError: Error?
    /// Holds `chatConnect` open (sink already registered) so a test can drop
    /// the leg while the dial's success is still unwinding.
    private var connectStallMs: Int = 0
    /// The same window for `chatSendAfterConnect` — the send slow path has its
    /// own success continuation with the same stale-success hazard.
    private var sendAfterConnectStallMs: Int = 0
    private var createSessionError: Error?
    private var syncOutcome: Result<String, Error> = .success(FakeBayboClient.emptySyncFrame)
    private var lookupResults: [String: MessageLookup] = [:]
    private var lookupError: Error?
    private var subagentList = ChatSubagentList(items: [], hasMoreOlder: false)
    private var subagentListError: Error?
    private var subagentListCalls: [String] = []
    /// Recorded search queries, in call order — what a debounce test asserts on.
    private var searches: [String] = []
    private var searchResults: [String: ChatSearchResults] = [:]
    private var searchError: Error?
    private var approvalError: Error?
    private var sessionModelPin = SessionModelPin(llm: nil, model: nil, effort: nil)
    private var sessionModelError: Error?
    private var sessionModelStallMs: Int = 0
    private var setModelError: Error?
    private var llmCatalog: LlmModelCatalog?

    /// The baseline answer to a sync: no rows, no cursor. Enough to unwind the
    /// webview's in-flight guard, and it confirms nothing in the outbox.
    static let emptySyncFrame = #"""
        {"kind":"sync_page","rows":[],"since_ordinal":null,"next_cursor":null,
         "rebased":false,"oldest_ordinal":null,"has_more_older":false}
        """#

    // MARK: - Recorded calls

    var sentMessages: [SendCall] { lock.withLock { sends } }
    var sentAfterConnect: [SendCall] { lock.withLock { sendAfterConnects } }
    var connectedSessions: [String] { lock.withLock { connects } }
    var createdSessionIds: [String] { lock.withLock { createdSessions } }
    var lookupCalls: [LookupCall] { lock.withLock { lookups } }
    /// Queries that reached the core, in order — a debounce or a
    /// skip-while-composing assertion is a statement about THIS array.
    var searchCalls: [String] { lock.withLock { searches } }
    var approvalCalls: [ApprovalCall] { lock.withLock { approvals } }
    var readOrdinals: [Int64] { lock.withLock { marksRead } }
    /// One entry per `chatMarkManyRead` call — the cron group's "mark all read"
    /// must be ONE round-trip, not one per fire.
    var batchReadCalls: [[String]] { lock.withLock { batchMarksRead } }
    var setModelCalls: [SetModelCall] { lock.withLock { modelSets } }
    var modelReadSessions: [String] { lock.withLock { modelReads } }
    var callTimeline: [String] { lock.withLock { callOrder } }
    /// Streaming deck picker uploads (`deck.pickBlob`'s file leg) — the path
    /// proves the bytes never crossed the FFI, the card id proves the blob is
    /// stamped `deck:<card>` and reclaimable at purge.
    var deckFileUploadCalls: [DeckFileUpload] { lock.withLock { deckFileUploads } }
    /// Composer uploads, in the order they reached the wire.
    var blobUploadCalls: [BlobUploadCall] { lock.withLock { blobUploads } }
    var blobDownloadCalls: [String] { lock.withLock { blobDownloads } }
    var blobCachedReadCalls: [String] { lock.withLock { blobCachedReads } }
    /// Composer uploads currently PARKED, i.e. the ones a one-at-a-time release
    /// can actually finish. The call log records an upload the moment it
    /// arrives, a beat before its continuation is registered — waiting on the
    /// log and then releasing one can therefore release nothing at all.
    var parkedBlobUploads: Int { lock.withLock { blobUploadWaiters.count } }
    /// How many times the app dropped its warm relay API legs — the `.background`
    /// barrier. iOS suspends without warning and takes the sockets with it, so a
    /// leg that outlives a suspend is a zombie.
    var apiLegInvalidationCount: Int { lock.withLock { apiLegInvalidations } }

    /// Every transmission of a message, however it reached the wire — the
    /// initial `chatSendAfterConnect` on a cold leg and every later `chatSend`.
    /// A duplicate durable send (the failure the reconcile gate exists to
    /// prevent) shows up here as two entries with the same `msgId`.
    var transmissions: [SendCall] {
        lock.withLock { sendAfterConnects + sends }
    }

    // MARK: - Canned behaviour

    func failConnect(with error: Error) { lock.withLock { connectError = error } }
    /// Clear a prior `failConnect` — the network came back.
    func succeedConnect() { lock.withLock { connectError = nil } }
    func failSend(with error: Error) { lock.withLock { sendError = error } }
    /// Fail only the connected fast path (`chatSend`); the dial-and-send slow
    /// path stays healthy. See `plainSendError`.
    func failPlainSend(with error: Error) { lock.withLock { plainSendError = error } }
    /// Hold each `chatConnect` open for `ms` after registering the sink — the
    /// window a test needs to drop the leg mid-dial.
    func stallConnect(ms: Int) { lock.withLock { connectStallMs = ms } }
    /// Hold each `chatSendAfterConnect` open for `ms` after registering the
    /// sink — the send slow path's mid-dial window.
    func stallSendAfterConnect(ms: Int) { lock.withLock { sendAfterConnectStallMs = ms } }
    func failCreateSession(with error: Error) { lock.withLock { createSessionError = error } }
    /// Clear a prior `failCreateSession` — simulates the network coming back so a
    /// stranded draft's next recovery attempt succeeds.
    func succeedCreateSession() { lock.withLock { createSessionError = nil } }
    func failApproval(with error: Error) { lock.withLock { approvalError = error } }
    /// Let the streaming deck upload succeed with `blobId` (it throws by
    /// default, like every other unstubbed call).
    func answerDeckFileUpload(with blobId: String) {
        lock.withLock { deckFileUploadBlobId = blobId }
    }

    /// The blob a composer upload lands as, unless `releaseBlobUploads` names
    /// another outcome.
    static let uploadedBlobId = "sha256:staged.tok"

    /// Park every composer upload until `releaseBlobUploads` — the window a
    /// test needs to remove a tile while its bytes are on the wire.
    /// Deliberately DEAF to cancellation, exactly like the real one: the
    /// generated UniFFI async binding has no cancellation hook, so a cancelled
    /// upload still runs to completion.
    func holdBlobUploads() { lock.withLock { blobUploadsHeld = true } }

    /// Finish every parked upload with `outcome`, and stop parking.
    func releaseBlobUploads(
        _ outcome: Result<String, Error> = .success(FakeBayboClient.uploadedBlobId)
    ) {
        let parked: [CheckedContinuation<Void, Never>] = lock.withLock {
            blobUploadsHeld = false
            blobUploadOutcome = outcome
            let waiters = blobUploadWaiters
            blobUploadWaiters.removeAll()
            return waiters
        }
        for waiter in parked { waiter.resume() }
    }

    /// Finish the OLDEST parked upload and keep holding the rest. How many
    /// picks a completion releases is the concurrency budget, and a blanket
    /// release starts every queued one at once — which is precisely the bug it
    /// would have to distinguish.
    func releaseOneBlobUpload(
        _ outcome: Result<String, Error> = .success(FakeBayboClient.uploadedBlobId)
    ) {
        let waiter: CheckedContinuation<Void, Never>? = lock.withLock {
            blobUploadOutcome = outcome
            return blobUploadWaiters.isEmpty ? nil : blobUploadWaiters.removeFirst()
        }
        waiter?.resume()
    }

    func answerSync(with frameJson: String) {
        lock.withLock { syncOutcome = .success(frameJson) }
    }

    func failSync(with error: Error) { lock.withLock { syncOutcome = .failure(error) } }

    /// The durability point lookup's answer for one key. An unstubbed key
    /// answers `found: false` — provably absent, which is what resumes the retry
    /// machine.
    func answerLookup(platformMsgId: String, found: Bool, ordinal: Int64? = nil) {
        lock.withLock {
            lookupResults[platformMsgId] = MessageLookup(found: found, ordinal: ordinal)
        }
    }

    func failLookup(with error: Error) { lock.withLock { lookupError = error } }

    func stubSearch(_ query: String, with results: ChatSearchResults) {
        lock.withLock { searchResults[query] = results }
    }

    func failSearch(with error: Error) { lock.withLock { searchError = error } }

    /// The session meta's pin answer for `chatSessionModel`.
    func answerSessionModel(llm: String?, model: String? = nil, effort: String? = nil) {
        lock.withLock { sessionModelPin = SessionModelPin(llm: llm, model: model, effort: effort) }
    }
    func failSessionModel(with error: Error) { lock.withLock { sessionModelError = error } }
    /// Hold each `chatSessionModel` answer back — for the race tests where a
    /// pin read must still be in flight when a selection lands.
    func stallSessionModel(ms: Int) { lock.withLock { sessionModelStallMs = ms } }
    func failSetModel(with error: Error) { lock.withLock { setModelError = error } }
    func answerModelCatalog(_ catalog: LlmModelCatalog) { lock.withLock { llmCatalog = catalog } }

    /// Push a frame into the session's live sink, exactly as the core's pump
    /// does. The sink hops to the main queue, so the caller must let the actor
    /// drain (`waitUntil`).
    func deliver(_ frameJson: String, to sessionId: String) {
        lock.withLock { sinks[sessionId] }?.onFrame(frameJson: frameJson)
    }

    /// Report the leg dead to the session's registered sink, exactly as the
    /// core's `on_disconnected` fan-out does (the FFI guarantees the delivery
    /// for every unsolicited pump death).
    func dropLeg(of sessionId: String) {
        lock.withLock { sinks[sessionId] }?.onDisconnected(sessionId: sessionId)
    }

    // MARK: - BayboClientProtocol

    func setBlobCacheDir(directory: String) {}

    func chatConnect(sessionId: String, sink: FrameSink) async throws {
        let error: Error? = lock.withLock {
            connects.append(sessionId)
            if connectError == nil { sinks[sessionId] = sink }
            return connectError
        }
        if let error { throw error }
        let stall = lock.withLock { connectStallMs }
        if stall > 0 { try? await Task.sleep(for: .milliseconds(stall)) }
    }

    func chatSendAfterConnect(
        sessionId: String, sink: FrameSink, text: String, msgId: String,
        attachments: [AttachmentRef]
    ) async throws {
        let error: Error? = lock.withLock {
            connects.append(sessionId)
            if let sendError { return sendError }
            sinks[sessionId] = sink
            sendAfterConnects.append(
                SendCall(
                    sessionId: sessionId, text: text, msgId: msgId, attachments: attachments))
            callOrder.append("sendAfterConnect")
            return nil
        }
        if let error { throw error }
        let stall = lock.withLock { sendAfterConnectStallMs }
        if stall > 0 { try? await Task.sleep(for: .milliseconds(stall)) }
    }

    func chatSend(
        sessionId: String, text: String, msgId: String, attachments: [AttachmentRef]
    ) async throws {
        let error: Error? = lock.withLock {
            if let plainSendError { return plainSendError }
            if let sendError { return sendError }
            sends.append(
                SendCall(
                    sessionId: sessionId, text: text, msgId: msgId, attachments: attachments))
            callOrder.append("send")
            return nil
        }
        if let error { throw error }
    }

    func chatCreateSession(sessionId: String) async throws -> String {
        let error: Error? = lock.withLock {
            createdSessions.append(sessionId)
            if createSessionError == nil { callOrder.append("create") }
            return createSessionError
        }
        if let error { throw error }
        return sessionId
    }

    func chatFetchSync(sessionId: String, sinceOrdinal: Int64?, limit: UInt32) async throws
        -> String
    {
        try lock.withLock { syncOutcome }.get()
    }

    func chatLookupMessage(sessionId: String, platformMsgId: String) async throws -> MessageLookup {
        let outcome: Result<MessageLookup, Error> = lock.withLock {
            lookups.append(LookupCall(sessionId: sessionId, platformMsgId: platformMsgId))
            if let lookupError { return .failure(lookupError) }
            return .success(lookupResults[platformMsgId] ?? MessageLookup(found: false, ordinal: nil))
        }
        return try outcome.get()
    }

    func chatSearch(query: String) async throws -> ChatSearchResults {
        let outcome: Result<ChatSearchResults, Error> = lock.withLock {
            searches.append(query)
            if let searchError { return .failure(searchError) }
            return .success(
                searchResults[query] ?? ChatSearchResults(groups: [], truncated: false))
        }
        return try outcome.get()
    }

    func chatResolveApproval(callId: String, decision: ApprovalDecision) async throws {
        let error: Error? = lock.withLock {
            approvals.append(ApprovalCall(callId: callId, decision: decision))
            return approvalError
        }
        if let error { throw error }
    }

    func chatMarkRead(sessionId: String, ordinal: Int64) async throws {
        lock.withLock { marksRead.append(ordinal) }
    }

    func chatMarkManyRead(sessionIds: [String]) async throws {
        lock.withLock { batchMarksRead.append(sessionIds) }
    }

    func chatUnsubscribe(sessionId: String) async {
        lock.withLock { _ = sinks.removeValue(forKey: sessionId) }
    }

    func chatDisconnect() async {
        lock.withLock { sinks.removeAll() }
    }

    func chatFetchHistory(sessionId: String, beforeOrdinal: Int64?, limit: UInt32?) async throws
        -> String
    {
        throw Self.unsupported
    }

    // The subagent read surface. `chatListSubagents` is programmable because the
    // read-only page's polling loop retires itself on what it returns; the two
    // transcript pulls follow `chatFetchHistory` and stay unsupported until a
    // test needs them.
    func chatListSubagents(sessionId: String, before: SubagentCursor?) async throws
        -> ChatSubagentList
    {
        let outcome: Result<ChatSubagentList, Error> = lock.withLock {
            subagentListCalls.append(sessionId)
            if let subagentListError { return .failure(subagentListError) }
            return .success(subagentList)
        }
        return try outcome.get()
    }

    func chatFetchSubagentHistory(
        sessionId: String, beforeOrdinal: Int64?, limit: UInt32?
    ) async throws -> String {
        throw Self.unsupported
    }

    func chatFetchSubagentSync(
        sessionId: String, sinceOrdinal: Int64?, limit: UInt32
    ) async throws -> String {
        throw Self.unsupported
    }

    func chatHideSession(sessionId: String) async throws { throw Self.unsupported }
    func chatHideMany(sessionIds: [String]) async throws { throw Self.unsupported }
    func chatListCronJobs() async throws -> [CronJobSummary] { throw Self.unsupported }
    func chatSetCronPaused(jobId: String, paused: Bool) async throws { throw Self.unsupported }
    func chatDeleteCronJob(jobId: String) async throws { throw Self.unsupported }
    func chatListSessions() async throws -> [ChatSessionSummary] { throw Self.unsupported }
    func chatSetArchived(sessionId: String, archived: Bool) async throws { throw Self.unsupported }
    func chatSetPinned(sessionId: String, pinned: Bool) async throws { throw Self.unsupported }
    func chatSetTitle(sessionId: String, title: String) async throws { throw Self.unsupported }

    func chatSetCronPinned(jobId: String, pinned: Bool) async throws { throw Self.unsupported }

    func llmListModels() async throws -> LlmModelCatalog {
        guard let catalog = lock.withLock({ llmCatalog }) else { throw Self.unsupported }
        return catalog
    }

    func chatSessionModel(sessionId: String) async throws -> SessionModelPin {
        let stall = lock.withLock { sessionModelStallMs }
        if stall > 0 {
            try? await Task.sleep(for: .milliseconds(stall))
        }
        let outcome: Result<SessionModelPin, Error> = lock.withLock {
            modelReads.append(sessionId)
            if let sessionModelError { return .failure(sessionModelError) }
            return .success(sessionModelPin)
        }
        return try outcome.get()
    }

    func chatSetSessionModel(sessionId: String, llm: String?, model: String?, effort: String?)
        async throws
    {
        let error: Error? = lock.withLock {
            modelSets.append(
                SetModelCall(sessionId: sessionId, llm: llm, model: model, effort: effort))
            if setModelError == nil { callOrder.append("setModel") }
            return setModelError
        }
        if let error { throw error }
    }

    // MARK: deck

    /// Scripted deck responses + recorded mutations, so `DeckStore`'s
    /// refresh / layout / action paths are testable with no gateway.
    var deckView: DeckView = DeckView(cards: [], snapshots: [])
    var deckRecycleList: [DeckCardInfo] = []
    private(set) var deckLayoutPuts: [[DeckLayoutEntryInput]] = []
    private(set) var deckEnableCalls: [(String, Bool)] = []
    private(set) var deckDeletes: [String] = []
    private(set) var deckRestores: [String] = []
    private(set) var deckPurges: [String] = []
    var deckLayoutError: Error?

    func deckFetch() async throws -> DeckView {
        lock.withLock { deckView }
    }

    func deckFetchRecycle() async throws -> [DeckCardInfo] {
        lock.withLock { deckRecycleList }
    }

    func deckFetchBundle(cardId: String) async throws -> String { throw Self.unsupported }

    func deckCall(cardId: String, op: String, paramsJson: String, retryable: Bool) async throws
        -> String
    {
        throw Self.unsupported
    }

    func deckSetLayout(entries: [DeckLayoutEntryInput]) async throws {
        let failure = lock.withLock { () -> Error? in
            deckLayoutPuts.append(entries)
            // An ACKed layout is what the NEXT fetch has to hand back. Only
            // recording the PUT leaves `deckView` at the pre-PUT order, so
            // `DeckStore`'s post-rollback `requestRefresh()` reads back a state
            // the server no longer holds and undoes the rollback it is meant to
            // confirm — a race the test can only win by finishing first.
            if deckLayoutError == nil { deckView = Self.applyingLayout(entries, to: deckView) }
            return deckLayoutError
        }
        if let failure { throw failure }
    }

    private static func applyingLayout(_ entries: [DeckLayoutEntryInput], to view: DeckView)
        -> DeckView
    {
        var byId: [String: DeckLayoutEntryInput] = [:]
        for entry in entries { byId[entry.cardId] = entry }
        let cards =
            view.cards
            .map { card -> DeckCardInfo in
                guard let entry = byId[card.cardId] else { return card }
                return DeckCardInfo(
                    cardId: card.cardId,
                    title: card.title,
                    position: entry.position,
                    size: entry.size,
                    sizes: card.sizes,
                    maximize: card.maximize,
                    enabled: card.enabled,
                    quarantined: card.quarantined,
                    deletedAtMs: card.deletedAtMs,
                    specHash: card.specHash,
                    lastSeq: card.lastSeq,
                    createdAtMs: card.createdAtMs,
                    retryableOps: card.retryableOps
                )
            }
            .sorted { $0.position < $1.position }
        return DeckView(cards: cards, snapshots: view.snapshots)
    }

    func deckSetEnabled(cardId: String, enabled: Bool) async throws {
        lock.withLock { deckEnableCalls.append((cardId, enabled)) }
    }

    func deckDelete(cardId: String) async throws {
        lock.withLock { deckDeletes.append(cardId) }
    }

    func deckRestore(cardId: String) async throws -> DeckCardInfo {
        try lock.withLock {
            deckRestores.append(cardId)
            guard let info = deckRecycleList.first(where: { $0.cardId == cardId }) else {
                throw Self.unsupported
            }
            deckRecycleList.removeAll { $0.cardId == cardId }
            return info
        }
    }

    func deckPurge(cardId: String) async throws {
        try lock.withLock {
            deckPurges.append(cardId)
            guard deckRecycleList.contains(where: { $0.cardId == cardId }) else {
                throw Self.unsupported
            }
            deckRecycleList.removeAll { $0.cardId == cardId }
        }
    }

    func setDeckSink(sink: DeckSink) {}

    // MARK: - Projects

    private var _stubProjects: [ProjectInfo] = []
    private var _stubIssues: [IssueInfo] = []
    private var _stubIssueDetail: IssueInfo?
    private var _issueDetailStallMs = 0
    private var _issueDetailCalls = 0
    private var _issueReads: [Int64] = []
    private var _stubIssueEventsJson: String?
    private var _stubRunLog: IssueRunLog?
    private var _approvalsResolved: [(Int64, String, IssueApprovalDecision)] = []
    private var _approvalResolveError: Error?
    private var _stubRuns: [IssueRunInfo] = []
    private var _stubTeam: [TeamMemberInfo] = []
    private var _projectTeamCalls = 0
    private var _stubAttention: [ProjectAttention] = []
    private var _stubActivity: [ProjectActivity] = []
    private var _patches: [(Int64, IssuePatch)] = []
    private var _comments: [CommentCall] = []
    private var _failComments = false
    private var commentsHeld = false
    private var commentWaiters: [CheckedContinuation<Void, Never>] = []
    private var _failProjects = false

    var patches: [(Int64, IssuePatch)] { lock.withLock { _patches } }

    var stubProjects: [ProjectInfo] {
        get { lock.withLock { _stubProjects } }
        set { lock.withLock { _stubProjects = newValue } }
    }
    var stubIssues: [IssueInfo] {
        get { lock.withLock { _stubIssues } }
        set { lock.withLock { _stubIssues = newValue } }
    }
    var stubIssueDetail: IssueInfo? {
        get { lock.withLock { _stubIssueDetail } }
        set { lock.withLock { _stubIssueDetail = newValue } }
    }
    var issueDetailStallMs: Int {
        get { lock.withLock { _issueDetailStallMs } }
        set { lock.withLock { _issueDetailStallMs = newValue } }
    }
    var issueDetailCalls: Int { lock.withLock { _issueDetailCalls } }
    var issueReads: [Int64] { lock.withLock { _issueReads } }
    var stubIssueEventsJson: String? {
        get { lock.withLock { _stubIssueEventsJson } }
        set { lock.withLock { _stubIssueEventsJson = newValue } }
    }
    var stubRunLog: IssueRunLog? {
        get { lock.withLock { _stubRunLog } }
        set { lock.withLock { _stubRunLog = newValue } }
    }
    var approvalsResolved: [(Int64, String, IssueApprovalDecision)] {
        get { lock.withLock { _approvalsResolved } }
        set { lock.withLock { _approvalsResolved = newValue } }
    }
    var approvalResolveError: Error? {
        get { lock.withLock { _approvalResolveError } }
        set { lock.withLock { _approvalResolveError = newValue } }
    }
    var stubRuns: [IssueRunInfo] {
        get { lock.withLock { _stubRuns } }
        set { lock.withLock { _stubRuns = newValue } }
    }
    var stubTeam: [TeamMemberInfo] {
        get { lock.withLock { _stubTeam } }
        set { lock.withLock { _stubTeam = newValue } }
    }
    var projectTeamCalls: Int { lock.withLock { _projectTeamCalls } }
    var stubAttention: [ProjectAttention] {
        get { lock.withLock { _stubAttention } }
        set { lock.withLock { _stubAttention = newValue } }
    }
    var stubActivity: [ProjectActivity] {
        get { lock.withLock { _stubActivity } }
        set { lock.withLock { _stubActivity = newValue } }
    }
    var failProjects: Bool {
        get { lock.withLock { _failProjects } }
        set { lock.withLock { _failProjects = newValue } }
    }
    var comments: [CommentCall] { lock.withLock { _comments } }
    var failComments: Bool {
        get { lock.withLock { _failComments } }
        set { lock.withLock { _failComments = newValue } }
    }
    var parkedComments: Int { lock.withLock { commentWaiters.count } }

    func holdComments() { lock.withLock { commentsHeld = true } }

    func releaseComments() {
        let parked: [CheckedContinuation<Void, Never>] = lock.withLock {
            commentsHeld = false
            let waiters = commentWaiters
            commentWaiters.removeAll()
            return waiters
        }
        for waiter in parked { waiter.resume() }
    }

    private func refuseIfOffline() throws {
        if lock.withLock({ _failProjects }) { throw BayboError.NotConnected }
    }

    func projectList(includeArchived: Bool) async throws -> [ProjectInfo] {
        try refuseIfOffline()
        return stubProjects
    }
    func projectGet(projectId: String) async throws -> ProjectInfo { throw Self.unsupported }
    func projectCreate(new: NewProject) async throws -> ProjectInfo { throw Self.unsupported }
    func projectUpdate(projectId: String, settings: ProjectSettings) async throws {
        throw Self.unsupported
    }
    func projectSetArchived(projectId: String, archived: Bool) async throws -> ProjectInfo {
        throw Self.unsupported
    }
    func projectIssues(projectId: String) async throws -> [IssueInfo] {
        try refuseIfOffline()
        return stubIssues
    }
    func projectIssueGet(projectId: String, number: Int64) async throws -> IssueInfo {
        try refuseIfOffline()
        let (detail, stall) = lock.withLock {
            _issueDetailCalls += 1
            return (_stubIssueDetail, _issueDetailStallMs)
        }
        if stall > 0 { try? await Task.sleep(for: .milliseconds(stall)) }
        guard let detail else { throw Self.unsupported }
        return detail
    }
    func projectIssueCreate(projectId: String, new: NewIssue) async throws -> IssueInfo {
        throw Self.unsupported
    }
    func projectIssuePatch(projectId: String, number: Int64, patch: IssuePatch) async throws
        -> IssueInfo
    {
        lock.withLock { _patches.append((number, patch)) }
        guard let card = stubIssueDetail else { throw Self.unsupported }
        return card
    }
    func projectIssueMove(
        projectId: String, number: Int64, status: IssueStatus, orderedNumbers: [Int64]
    ) async throws -> IssueInfo { throw Self.unsupported }
    func projectActiveRuns(projectId: String) async throws -> [IssueRunInfo] {
        try refuseIfOffline()
        return stubRuns
    }
    func projectIssueRuns(projectId: String, number: Int64) async throws -> IssueRunLog {
        try refuseIfOffline()
        guard let stubRunLog else { throw Self.unsupported }
        return stubRunLog
    }
    func projectRunCancel(projectId: String, number: Int64) async throws { throw Self.unsupported }
    func projectRunRetry(projectId: String, number: Int64) async throws -> IssueRunInfo {
        try refuseIfOffline()
        return IssueRunInfo(
            number: number, attempt: 1, agentId: "a-dev", status: .queued, trigger: .retry,
            sessionId: nil, error: nil, createdAtMs: 0, startedAtMs: nil, settledAtMs: nil,
            costMicros: nil, inputTokens: nil, outputTokens: nil)
    }
    var stubRunBaselineJson: String?
    var stubRunHistoryJson: String?
    var runHistoryAsks: [Int64?] = []
    var runBaselineAsks = 0
    func projectRunTranscript(
        projectId: String, number: Int64, attempt: Int64, beforeOrdinal: Int64?, limit: UInt32?
    ) async throws -> String {
        runHistoryAsks.append(beforeOrdinal)
        try refuseIfOffline()
        guard let stubRunHistoryJson else { throw Self.unsupported }
        return stubRunHistoryJson
    }
    func projectRunTranscriptBaseline(
        projectId: String, number: Int64, attempt: Int64, limit: UInt32?
    ) async throws -> String {
        runBaselineAsks += 1
        try refuseIfOffline()
        guard let stubRunBaselineJson else { throw Self.unsupported }
        return stubRunBaselineJson
    }
    func projectIssueEvents(projectId: String, number: Int64) async throws -> String {
        try refuseIfOffline()
        guard let stubIssueEventsJson else { throw Self.unsupported }
        return stubIssueEventsJson
    }
    func projectIssueComment(
        projectId: String, number: Int64, clientMsgId: String, text: String,
        attachments: [IssueAttachmentInput]
    ) async throws -> String {
        try refuseIfOffline()
        let held: Bool = lock.withLock {
            _comments.append(
                CommentCall(
                    number: number, clientMsgId: clientMsgId, text: text, attachments: attachments))
            return commentsHeld
        }
        if held {
            await withCheckedContinuation { continuation in
                let alreadyReleased: Bool = lock.withLock {
                    guard commentsHeld else { return true }
                    commentWaiters.append(continuation)
                    return false
                }
                if alreadyReleased { continuation.resume() }
            }
        }
        if failComments { throw Self.unsupported }
        let wireAttachments: [[String: Any]] = attachments.map { attachment in
            var wire: [String: Any] = [
                "blob_id": attachment.blobId,
                "mime_type": "application/octet-stream",
                "size": 0,
            ]
            if let filename = attachment.filename { wire["filename"] = filename }
            return wire
        }
        let row: [String: Any] = [
            "id": "event-\(clientMsgId)",
            "client_msg_id": clientMsgId,
            "number": number,
            "actor": ["kind": "user"],
            "body": [
                "kind": "comment",
                "text": text,
                "attachments": wireAttachments,
            ],
            "created_at_ms": 1,
        ]
        let data = try JSONSerialization.data(withJSONObject: row)
        guard let json = String(data: data, encoding: .utf8) else { throw Self.unsupported }
        return json
    }
    func projectIssueApprovalResolve(
        projectId: String, number: Int64, callId: String, decision: IssueApprovalDecision
    ) async throws {
        try refuseIfOffline()
        let error = lock.withLock {
            _approvalsResolved.append((number, callId, decision))
            return _approvalResolveError
        }
        if let error { throw error }
    }
    func projectIssueRead(projectId: String, number: Int64) async throws {
        try refuseIfOffline()
        lock.withLock { _issueReads.append(number) }
    }
    func projectRead(projectId: String) async throws { try refuseIfOffline() }
    func projectFeed(projectId: String, beforeMs: Int64?, limit: UInt32?) async throws -> String {
        throw Self.unsupported
    }
    func projectTeam(projectId: String) async throws -> [TeamMemberInfo] {
        lock.withLock { _projectTeamCalls += 1 }
        try refuseIfOffline()
        return stubTeam
    }
    func projectRemoveAgent(projectId: String, agentId: String) async throws {
        throw Self.unsupported
    }
    var avatarsSet: [(agentId: String, blobId: String?)] = []
    func agentSetAvatar(agentId: String, blobId: String?) async throws {
        try refuseIfOffline()
        avatarsSet.append((agentId, blobId))
    }
    var generatedAvatarsSet: [(agentId: String, blobId: String)] = []
    func agentSetAvatarIfEmpty(agentId: String, blobId: String) async throws {
        try refuseIfOffline()
        generatedAvatarsSet.append((agentId, blobId))
    }

    func agentSetModel(
        agentId: String, llm: String?, model: String?, reasoningEffort: String?
    ) async throws {
        throw Self.unsupported
    }
    func projectsAttention() async throws -> [ProjectAttention] {
        try refuseIfOffline()
        return stubAttention
    }
    func projectsActivity(sinceMs: Int64?) async throws -> [ProjectActivity] {
        try refuseIfOffline()
        return stubActivity
    }
    func setProjectSink(sink: ProjectSink) {}

    /// Answers from the same seeded cache the cached-read paths use, so a test
    /// that wants a tapped attachment to have bytes seeds one map, not two.
    func blobDownloadBytes(blobId: String, progress: BlobProgress?) async throws -> Data {
        lock.withLock { blobDownloads.append(blobId) }
        guard let data = downloadableBlobs[blobId] ?? cachedBlobs[blobId] else {
            throw Self.unsupported
        }
        return data
    }

    func blobIsCached(blobId: String) async -> Bool { cachedBlobs[blobId] != nil }

    /// Seeded cache for tests (`deck.shareBlob` / display fast path).
    var cachedBlobs: [String: Data] = [:]
    var downloadableBlobs: [String: Data] = [:]
    func blobReadCached(blobId: String) async -> Data? {
        lock.withLock { blobCachedReads.append(blobId) }
        return cachedBlobs[blobId]
    }

    func blobBytesForDisplay(blobId: String, maxBytes: UInt64) async -> BlobServeOutcome {
        guard let data = cachedBlobs[blobId] else { return .notFound }
        return UInt64(data.count) > maxBytes ? .overCap : .bytes(data: data)
    }

    func blobUploadBytes(bytes: Data, mimeType: String) async throws -> String {
        throw Self.unsupported
    }

    func deckBlobUploadBytes(bytes: Data, mimeType: String, cardId: String) async throws -> String {
        throw Self.unsupported
    }

    func blobUploadFile(path: String, mimeType: String, progress: BlobProgress?) async throws
        -> String
    {
        let held: Bool = lock.withLock {
            blobUploads.append(BlobUploadCall(path: path, mimeType: mimeType))
            return blobUploadsHeld
        }
        if held {
            await withCheckedContinuation { continuation in
                // The release can land between the check above and here.
                let alreadyReleased: Bool = lock.withLock {
                    guard blobUploadsHeld else { return true }
                    blobUploadWaiters.append(continuation)
                    return false
                }
                if alreadyReleased { continuation.resume() }
            }
        }
        return try lock.withLock { blobUploadOutcome }.get()
    }

    func deckBlobUploadFile(
        path: String, mimeType: String, cardId: String, progress: BlobProgress?
    ) async throws -> String {
        let blobId = lock.withLock { () -> String? in
            deckFileUploads.append(DeckFileUpload(path: path, mimeType: mimeType, cardId: cardId))
            return deckFileUploadBlobId
        }
        guard let blobId else { throw Self.unsupported }
        return blobId
    }

    func directLogin(baseUrl: String, token: String) async throws -> String {
        throw Self.unsupported
    }

    func directPreconnect() async throws { throw Self.unsupported }
    func directStatus() throws -> String? { nil }
    func logout() async throws { throw Self.unsupported }

    func pairBegin(target: PairTarget, onAbort: PairAbortListener) async throws -> PairChallenge {
        throw Self.unsupported
    }

    func pairConfirm(deviceId: String, accepted: Bool) async throws -> PairedSummary {
        throw Self.unsupported
    }

    func pairedDevice() -> String? { nil }
    func registerPush() async throws -> String? { throw Self.unsupported }
    func relayInvalidateApiLegs() { lock.withLock { apiLegInvalidations += 1 } }
    func relayPreconnect() async throws { throw Self.unsupported }
    func setPushToken(token: PushToken) {}
    func setSessionListSink(sink: SessionListSink) {}
}
