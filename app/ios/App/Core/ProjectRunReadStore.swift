import Foundation

@MainActor
/// Read-only transcript target: it has no dial, outbox, draft, or disk mirror,
/// so stale run output cannot outlive a corrected server response.
final class ProjectRunReadStore: ObservableObject, TranscriptTarget {
    let projectId: String
    let number: Int64
    let attempt: Int64

    /// The run's own session, when it has one. Used as the transcript's key —
    /// the React tree is keyed on it, so two attempts never share a tree.
    let sessionId: String
    /// Nothing dials for a run, so the web side's epoch never advances. It
    /// exists to satisfy the one `init` payload field.
    let connEpoch = 0
    /// A run is deliberately absent from the chat list. The bridge reads this
    /// only to decide whether to hold the first paint.
    let listed = false
    let expandsUnansweredTail = true
    let mirrored = false

    /// Whether the run can still gain rows. Drives the poll's stop condition
    /// and the header's word.
    @Published private(set) var status: RunStatus

    @Published var filePreview: FilePreview?
    @Published var fileShare: FilePreview?
    @Published var viewedImage: ViewedImage?
    @Published var videoPlayback: VideoPlayback?

    private let client: any BayboClientProtocol
    private weak var bridge: TranscriptBridge?
    private var poll: Task<Void, Never>?
    private var invalidations: ProjectInvalidations.Token?

    private lazy var media: TranscriptMedia = {
        let media = TranscriptMedia(client: client)
        media.onPreview = { [weak self] in self?.filePreview = $0 }
        media.onShare = { [weak self] in self?.fileShare = $0 }
        media.onViewImage = { [weak self] in self?.viewedImage = $0 }
        media.onPlayVideo = { [weak self] in self?.videoPlayback = $0 }
        return media
    }()

    private static let pollInterval = Duration.seconds(2)

    init(
        projectId: String, number: Int64, attempt: Int64, sessionId: String,
        status: RunStatus, client: any BayboClientProtocol = Baybo.client
    ) {
        self.projectId = projectId
        self.number = number
        self.attempt = attempt
        self.sessionId = sessionId
        self.status = status
        self.client = client
    }

    func attachBridge(_ bridge: TranscriptBridge) {
        self.bridge = bridge
        media.attach(bridge)
        // A frame is what should normally drive a live run's page; the poll is
        // the fallback for when the device channel is not carrying them.
        invalidations = ProjectInvalidations.shared.observe { [weak self] change in
            guard let self else { return }
            guard change.scope == "stale" || change.projectId == self.projectId else { return }
            guard change.issueNumber == nil || change.issueNumber == self.number else { return }
            self.runChanged()
        }
        startPollingIfLive()
    }

    func detachBridge(_ bridge: TranscriptBridge) {
        guard self.bridge === bridge else { return }
        invalidations = nil
        self.bridge = nil
        media.detach(bridge)
        stopPolling()
    }

    // MARK: - Reading
    // Runs have no sync endpoint. Initial loads still need a `sync_page` frame;
    // backward scrolling needs `history_page`, because the web guards differ.

    func requestSync(sinceOrdinal: Int64?, limit: UInt32) {
        Task { [projectId, number, attempt] in
            do {
                let frame = try await client.projectRunTranscriptBaseline(
                    projectId: projectId, number: number, attempt: attempt, limit: limit)
                bridge?.pushFrame(frame)
            } catch {
                NSLog("baybo: run baseline: %@", bayboErrorText(error))
                // The webview armed an in-flight guard for this request and
                // will not ask again until something unwinds it.
                pushSynthesized(["kind": "sync_failed", "error": bayboErrorText(error)])
            }
        }
    }

    func fetchHistory(beforeOrdinal: Int64?, limit: UInt32) {
        Task { [projectId, number, attempt] in
            do {
                let frame = try await client.projectRunTranscript(
                    projectId: projectId, number: number, attempt: attempt,
                    beforeOrdinal: beforeOrdinal, limit: limit)
                bridge?.pushFrame(frame)
            } catch {
                NSLog("baybo: run history: %@", bayboErrorText(error))
                pushSynthesized(["kind": "history_failed", "error": bayboErrorText(error)])
            }
        }
    }

    // MARK: - Liveness

    func runChanged() {
        bridge?.requestSync()
    }

    func startPollingIfLive() {
        guard Self.isLive(status), poll == nil else { return }
        poll = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.pollInterval)
                guard let self, !Task.isCancelled else { return }
                self.bridge?.requestSync()
                if await self.refreshStatus() == false { return }
            }
        }
    }

    func stopPolling() {
        poll?.cancel()
        poll = nil
    }

    private func refreshStatus() async -> Bool {
        do {
            let log = try await client.projectIssueRuns(projectId: projectId, number: number)
            guard let mine = log.runs.first(where: { $0.attempt == attempt }) else { return true }
            let wasLive = Self.isLive(status)
            status = mine.status
            if Self.isLive(mine.status) { return true }
            // Settled on THIS tick: take one more page, then stop.
            if wasLive { bridge?.requestSync() }
            return false
        } catch {
            return true
        }
    }

    static func isLive(_ status: RunStatus) -> Bool {
        // Unknown is terminal for this build; a future live state must update
        // this gate or polling would stop too early.
        switch status {
        case .queued, .held, .running: true
        case .done, .failed, .cancelled, .unknown: false
        }
    }

    func replayUnconfirmedSends(to transcript: any TranscriptSurface) {}
    func flushPendingSendConfirms(to transcript: any TranscriptSurface) {}

    private func pushSynthesized(_ payload: [String: Any]) {
        if let data = try? JSONSerialization.data(withJSONObject: payload),
            let json = String(data: data, encoding: .utf8)
        {
            bridge?.pushFrame(json)
        }
    }

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
