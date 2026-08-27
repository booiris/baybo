import Foundation

/// The `TranscriptTarget` behind a subagent child's read-only page.
///
/// Everything it does is a GET. There is no dial, no outbox, no draft, no
/// unread cursor and no composer — which is why this is a separate type rather
/// than a flag on `ChatStore`: each of those has a guard in that class whose
/// wrong answer for an unlisted session is SILENT (`app/ios/docs/subagents.md`
/// lists them; the worst synthesizes an empty page and REPLACEs the thread
/// with it, so the transcript renders blank while looking healthy).
///
/// Attachments are NOT reimplemented here — `TranscriptMedia` is the same
/// engine the live chat uses, so a file card behaves identically in both.
@MainActor
final class SubagentReadStore: ObservableObject, TranscriptTarget {
    let sessionId: String
    /// Nothing dials for a child, so the web side's epoch never advances. It
    /// exists to satisfy the one `init` payload field.
    let connEpoch = 0
    /// A child is deliberately absent from the chat list. The bridge reads this
    /// only to decide whether to hold the first paint, and `false` is both true
    /// and the behaviour we want (fade in once the page reports `shown`).
    let listed = false
    let expandsUnansweredTail = true
    /// No mirror, enforced (docs/subagents.md always claimed it; the bridge
    /// used to read/write one anyway): a child page rendered against an old
    /// gateway would otherwise restore rows the fixed read path no longer
    /// serves, forever — the cursor covers the thread, so no sync removes them.
    let mirrored = false

    /// The child's state as its parent's listing last reported it. Drives the
    /// polling loop's stop condition and the page's header label.
    @Published private(set) var status: ChatSubagentStatus
    /// A tapped file attachment, materialised on disk and awaiting presentation.
    @Published var filePreview: FilePreview?
    /// A long-pressed attachment awaiting the system share sheet.
    @Published var fileShare: FilePreview?
    /// A tapped image attachment, decoded and awaiting the full-screen viewer.
    @Published var viewedImage: ViewedImage?
    /// A tapped video attachment, awaiting the full-screen player.
    @Published var videoPlayback: VideoPlayback?

    /// The conversation whose listing carries this child's status — the only
    /// place its liveness can be re-read, since a child's own session row never
    /// moves after creation.
    let parentSessionId: String

    private let client: any BayboClientProtocol
    private weak var bridge: TranscriptBridge?
    private var poll: Task<Void, Never>?

    private lazy var media: TranscriptMedia = {
        let media = TranscriptMedia(client: client)
        media.onPreview = { [weak self] in self?.filePreview = $0 }
        media.onShare = { [weak self] in self?.fileShare = $0 }
        media.onViewImage = { [weak self] in self?.viewedImage = $0 }
        media.onPlayVideo = { [weak self] in self?.videoPlayback = $0 }
        return media
    }()

    /// How often a running child re-syncs. A cursor difference over a child
    /// that produced nothing costs a request, not a page — and a child only
    /// ever advances in whole persisted rows: the `subagent` channel is not
    /// installed, so `build_history_page` folds no in-flight steps and there is
    /// nothing finer to poll for (see `app/ios/docs/subagents.md`).
    private static let pollInterval = Duration.seconds(3)

    init(
        sessionId: String, parentSessionId: String, status: ChatSubagentStatus,
        client: any BayboClientProtocol
    ) {
        self.sessionId = sessionId
        self.parentSessionId = parentSessionId
        self.status = status
        self.client = client
    }

    func attachBridge(_ bridge: TranscriptBridge) {
        self.bridge = bridge
        media.attach(bridge)
    }

    func detachBridge(_ bridge: TranscriptBridge) {
        guard self.bridge === bridge else { return }
        self.bridge = nil
        media.detach(bridge)
        stopPolling()
    }

    // MARK: - Reading

    func requestSync(sinceOrdinal: Int64?, limit: UInt32) {
        Task {
            do {
                let frame = try await client.chatFetchSubagentSync(
                    sessionId: sessionId, sinceOrdinal: sinceOrdinal, limit: limit)
                bridge?.pushFrame(frame)
            } catch {
                NSLog("baybo: subagent sync: %@", bayboErrorText(error))
                // The webview armed an in-flight guard for this request and
                // will not run its sync loop again until something unwinds it.
                pushSynthesized(["kind": "sync_failed", "error": bayboErrorText(error)])
            }
        }
    }

    func fetchHistory(beforeOrdinal: Int64?, limit: UInt32) {
        Task {
            do {
                let frame = try await client.chatFetchSubagentHistory(
                    sessionId: sessionId, beforeOrdinal: beforeOrdinal, limit: limit)
                bridge?.pushFrame(frame)
            } catch {
                NSLog("baybo: subagent history: %@", bayboErrorText(error))
                pushSynthesized(["kind": "history_failed", "error": bayboErrorText(error)])
            }
        }
    }

    // MARK: - Polling

    /// Re-sync while the child can still gain rows. A child that has ended
    /// never will, so a timer against one is pure battery — and the loop
    /// retires itself the moment the parent's listing says so.
    func startPollingIfLive() {
        guard status.isLive, poll == nil else { return }
        poll = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.pollInterval)
                guard let self, !Task.isCancelled else { return }
                // Ask the WEBVIEW to sync rather than syncing at it: the page
                // owns the cursor, so this keeps one source of truth for where
                // the thread is — the same move the live chat's re-attach makes.
                self.bridge?.requestSync()
                if await self.refreshStatus() == false { return }
            }
        }
    }

    func stopPolling() {
        poll?.cancel()
        poll = nil
    }

    /// Re-read this child's status from its parent's listing. Returns whether
    /// polling should continue; a transient failure keeps the loop alive,
    /// because the child may well still be working and the next tick is cheap.
    private func refreshStatus() async -> Bool {
        do {
            // The newest page is enough: a child polls its OWN status, and a
            // child old enough to have fallen off that page is old enough to
            // have ended long ago.
            let list = try await client.chatListSubagents(
                sessionId: parentSessionId, before: nil)
            guard let mine = list.items.first(where: { $0.sessionId == sessionId }) else {
                return true
            }
            status = mine.status
            return mine.status.isLive
        } catch {
            return true
        }
    }

    private func pushSynthesized(_ payload: [String: Any]) {
        if let data = try? JSONSerialization.data(withJSONObject: payload),
            let json = String(data: data, encoding: .utf8)
        {
            bridge?.pushFrame(json)
        }
    }

    // MARK: - Attachments

    func requestBlob(id: Int, blobId: String) {
        media.requestBlob(id: id, blobId: blobId)
    }

    func queryFileState(blobId: String) {
        media.queryFileState(blobId: blobId)
    }

    func downloadFile(blobId: String) {
        media.downloadFile(blobId: blobId)
    }

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

    func queryAudioState(blobId: String) {
        media.queryAudioState(blobId: blobId)
    }
}

extension ChatSubagentStatus {
    /// Whether the child can still gain rows — the polling loop's whole
    /// condition. `pending` counts: its actor exists and is about to open a
    /// turn. `unknown` does NOT, and that is a deliberate risk the FFI's own
    /// doc names: a future non-terminal status would decode here and freeze
    /// such a child's page until it is reopened.
    var isLive: Bool {
        switch self {
        case .pending, .running: true
        case .completed, .failed, .cancelled, .unknown: false
        }
    }
}
