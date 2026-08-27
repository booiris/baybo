import Foundation

/// The `TranscriptTarget` behind one run's transcript.
///
/// A card's run IS a session, and its transcript is the same rows the chat
/// renders — so the same webview and the same React tree draw it. What differs
/// is everything around them, which is why this is its own target rather than
/// a flag: there is no dial, no outbox, no draft, no composer and no unread
/// cursor, and each of those has a guard in `ChatStore` whose wrong answer for
/// a run would be SILENT.
///
/// **Not mirrored**, for the reason `SubagentReadStore` is not: a run's
/// transcript is a GET away, and a mirror is how a rendering the server no
/// longer produces outlives the fix that removed it. The cursor would cover
/// the thread, so no later sync could ever delete the stale rows.
@MainActor
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
    /// A run can settle or pause without a final assistant output. Its REST
    /// work rows are all closed, so the transcript opens that unanswered tail
    /// instead of presenting it as a completed `Worked` summary.
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

    /// The fallback cadence when no frame arrives to say the run moved.
    ///
    /// A run's page is normally driven by `ProjectChanged{run|timeline}` — a
    /// frame per step, which is far better than a timer. This is what runs when
    /// the device channel is not carrying them (a relay leg mid-reconnect, an
    /// older gateway): slow enough not to be a battery drain on a long run,
    /// fast enough that a page left open does not look frozen.
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
    //
    // A run has no SYNC route — only backward pages. So `requestSync` asks for
    // the NEWEST page rather than a difference: a run only ever grows at the
    // tail, and passing the cursor would ask the gateway a question it has no
    // route to answer.
    //
    // **The two calls take different doors, and the difference is the frame
    // KIND.** The web arms a guard on a sync request that only `sync_page` /
    // `sync_failed` unwinds, and separately DROPS a `history_page` matching no
    // in-flight backward-paging request. Answering the initial load with a
    // history page — which is what this did until 2026-08-26 — lost the rows
    // AND left the guard armed, so every run sat on "Loading conversation…"
    // with its transcript already fetched. Which door was taken is what decides
    // the failure frame too: a nil `beforeOrdinal` is an ordinary first
    // scroll-up page, so it cannot be the thing that tells the two apart.

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

    /// A frame said this run moved. Re-read the newest page rather than
    /// waiting for the poll — this is the path that should normally carry a
    /// live run, and the timer is only the fallback.
    func runChanged() {
        bridge?.requestSync()
    }

    func startPollingIfLive() {
        guard Self.isLive(status), poll == nil else { return }
        poll = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.pollInterval)
                guard let self, !Task.isCancelled else { return }
                // Ask the WEBVIEW to sync rather than syncing at it: the page
                // owns where the thread is, and this keeps one source of truth
                // for it — the live chat's re-attach makes the same move.
                self.bridge?.requestSync()
                if await self.refreshStatus() == false { return }
            }
        }
    }

    func stopPolling() {
        poll?.cancel()
        poll = nil
    }

    /// Re-read this run's status from the card's log. Returns whether polling
    /// should continue; a transient failure keeps the loop alive, because the
    /// run may well still be going and the next tick is cheap.
    ///
    /// One read after it settles, deliberately: a run that just finished has
    /// its last rows written after the settle stamp, so stopping at the stamp
    /// would leave the final answer off the page.
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

    /// Whether the run can still gain rows.
    ///
    /// `unknown` counts as NOT live, and that is the deliberate risk the FFI's
    /// own doc names: a future non-terminal status would decode here and
    /// freeze such a run's page until it is reopened.
    static func isLive(_ status: RunStatus) -> Bool {
        switch status {
        case .queued, .held, .running: true
        case .done, .failed, .cancelled, .unknown: false
        }
    }

    /// A run takes no sends, so there is nothing to replay and nothing to
    /// confirm. Empty rather than absent: the protocol is the chat's, and a
    /// `fatalError` here would be a crash waiting for whichever path forgets
    /// this target is read-only.
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
    //
    // The shared engine's, unchanged — a file card in a run's transcript
    // behaves exactly as it does in a conversation.

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
