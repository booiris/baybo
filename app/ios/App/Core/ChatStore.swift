import SwiftUI

/// The chat screen's connection state machine + send path — the native owner of
/// everything the webview's reconnect effect used to do (App.tsx 903-984).
///
/// Contract preserved from the web implementation:
/// * `offline` has exactly one trigger: a failed dial. An unsolicited pump
///   death (`onDisconnected`) flips back to `connecting` and schedules a
///   backoff redial; deliberate disconnect never fires the callback.
/// * Foreground reconnects are debounced (400ms) and coalesce with the dial
///   already in flight (the core's registry also coalesces).
/// * A dial generation guards late callbacks from a superseded sink.
@MainActor
final class ChatStore: ObservableObject {
    enum ConnState {
        case draft
        case connecting
        case connected
        case offline
    }

    static let reconnectBackoff: Duration = .milliseconds(2000)
    static let foregroundDebounce: Duration = .milliseconds(400)
    /// Matches the gateway's 100 MiB blob cap (`MAX_BLOB_BYTES`) so an
    /// over-size pick is rejected up front instead of failing after upload.
    static let maxAttachmentBytes = 100 * 1024 * 1024
    /// Ceiling on the offscreen frame buffer. A long agent turn on a
    /// backgrounded session streams every delta as a JSON string into
    /// `bufferedFrames`; past this the buffer is dropped and the transcript
    /// refetched on re-attach (`overflowBufferedFrames`) instead of growing
    /// without bound. Generous enough that a normal turn flushes intact.
    static let maxBufferedFrames = 2000

    let sessionId: String
    @Published private(set) var connState: ConnState
    /// Transient composer notice (send failed / waiting for upload / too large).
    @Published var notice: String?

    /// Increments on every successful dial; the webview uses it to retry
    /// attachments that raced ahead of the leg going live, and to drop late
    /// history frames from a superseded connection.
    private(set) var connEpoch = 0
    /// Newest-edge ordinal reported by the webview — the reconnect catch-up
    /// cursor (`sinceOrdinal`). In-memory only: the DURABLE cursor lives inside
    /// the persisted transcript blob (written atomically with the messages it
    /// matches), so a kill can never leave the cursor durably ahead of the
    /// transcript — the old localStorage blob had the same one-write property.
    private(set) var lastOrdinal: Int64?
    private var remoteSessionEnsured: Bool
    private var ensureRemoteSessionTask: Task<Void, Error>?

    /// Accepted floor: sinks below this are muted. Advanced when a dial
    /// actually REPLACES the pump (success) or on deliberate disconnect — NOT
    /// at dial start, so the still-live prior pump keeps rendering through the
    /// redial window (streamed deltas aren't durable rows; dropping them leaves
    /// holes).
    private var generation = 0
    /// Last generation handed to a dial's sink.
    private var issuedGeneration = 0
    private var retryTask: Task<Void, Never>?
    private var debounceTask: Task<Void, Never>?
    private var catchUpTask: Task<Void, Never>?
    private var connectTask: Task<Void, Error>?
    private weak var bridge: TranscriptBridge?
    private var bufferedFrames: [String] = []
    /// Set when the offscreen buffer overflowed and was dropped: the next
    /// `attachBridge` refetches the gap instead of flushing a hole-punched
    /// stream, and frames arriving meanwhile are dropped (not re-buffered) so
    /// the rewound catch-up cursor stays put.
    private var needsHistoryReset = false

    init(sessionId: String) {
        self.sessionId = sessionId
        let listed = SessionIndex.shared.contains(sessionId: sessionId)
        connState = listed ? .connecting : .draft
        remoteSessionEnsured = false
        lastOrdinal = Self.persistedOrdinal(sessionId: sessionId)
    }

    /// Whether a webview bridge is attached — i.e. this store is rendering on
    /// screen. LRU eviction skips these (`AppStore.evictIdleStores`).
    var hasBridge: Bool { bridge != nil }

    // MARK: - Connection lifecycle

    func connectIfNeeded() {
        guard connState != .connected, connState != .draft else { return }
        connect()
    }

    func connect() {
        guard connState != .connected, connState != .draft else { return }
        let task = startConnect()
        Task {
            do {
                try await task.value
            } catch {}
        }
    }

    @discardableResult
    private func startConnect() -> Task<Void, Error> {
        if let connectTask {
            return connectTask
        }
        retryTask?.cancel()
        retryTask = nil
        catchUpTask?.cancel()
        catchUpTask = nil
        let sinceOrdinal = lastOrdinal
        if connState != .connected {
            connState = .connecting
        }
        // A redial supersedes any lingering transient notice (old contract).
        notice = nil
        issuedGeneration += 1
        let gen = issuedGeneration
        let task = Task {
            defer {
                if issuedGeneration == gen {
                    connectTask = nil
                }
            }
            do {
                try await Baybo.client.chatConnect(
                    sessionId: sessionId,
                    sinceOrdinal: sinceOrdinal,
                    sink: Sink(store: self, generation: gen)
                )
                guard gen >= generation else { return }  // superseded by disconnect
                // The subscribe is now accepted; only NOW mute older sinks.
                generation = gen
                connEpoch += 1
                connState = .connected
                bridge?.setConnEpoch(connEpoch)
                if let sinceOrdinal {
                    fetchCatchUp(sinceOrdinal: sinceOrdinal, generation: gen)
                }
            } catch {
                guard gen >= generation else { return }
                // The one place `offline` is set — a failed dial.
                connState = .offline
                scheduleRetry()
                throw error
            }
        }
        connectTask = task
        return task
    }

    private func fetchCatchUp(sinceOrdinal: Int64, generation gen: Int) {
        catchUpTask?.cancel()
        catchUpTask = Task {
            do {
                let frame = try await Baybo.client.chatCatchUp(
                    sessionId: sessionId, sinceOrdinal: sinceOrdinal)
                guard !Task.isCancelled, gen >= generation else { return }
                pushFrame(frame)
            } catch {
                guard !Task.isCancelled else { return }
                NSLog("baybo: catchUp: %@", bayboErrorText(error))
            }
        }
    }

    /// Foreground / visibility signal: debounce, then redial (a no-op when the
    /// live session is healthy — the gateway replays only the gap).
    func scheduleReconnect() {
        guard connState != .draft else { return }
        debounceTask?.cancel()
        debounceTask = Task {
            try? await Task.sleep(for: Self.foregroundDebounce)
            guard !Task.isCancelled else { return }
            connect()
        }
    }

    private func scheduleRetry() {
        retryTask?.cancel()
        retryTask = Task {
            try? await Task.sleep(for: Self.reconnectBackoff)
            guard !Task.isCancelled else { return }
            connect()
        }
    }

    /// The pump died on its own (peer closed / liveness lapse / Noise desync).
    private func pumpDisconnected(sessionId: String, generation: Int) {
        guard sessionId == self.sessionId, generation >= self.generation else { return }
        connState = .connecting
        scheduleRetry()
    }

    /// Binding teardown (logout/rebind): cancel timers and drop the global pump
    /// deliberately, so the disconnected callback does not fire.
    func disconnect() async {
        retryTask?.cancel()
        retryTask = nil
        debounceTask?.cancel()
        debounceTask = nil
        catchUpTask?.cancel()
        catchUpTask = nil
        connectTask?.cancel()
        connectTask = nil
        // Mute every sink, including one handed to a dial still in flight.
        issuedGeneration += 1
        generation = issuedGeneration
        connState = remoteSessionEnsured ? .connecting : .draft
        await Baybo.client.chatDisconnect()
    }

    /// LRU eviction: this store is idle and offscreen. Cancel its timers and
    /// drop its gateway sink WITHOUT tearing the shared leg down (unlike
    /// `disconnect`, a binding-wide teardown) — every other subscribed session
    /// stays live. With its timers cancelled the store can deallocate; re-opening
    /// the session mints a fresh one that re-subscribes, and the transcript
    /// mirror + gateway history replay make that a cheap catch-up. The generation
    /// bump mutes any late sink callback that raced the unsubscribe.
    func evict() async {
        retryTask?.cancel()
        retryTask = nil
        debounceTask?.cancel()
        debounceTask = nil
        catchUpTask?.cancel()
        catchUpTask = nil
        connectTask?.cancel()
        connectTask = nil
        issuedGeneration += 1
        generation = issuedGeneration
        await Baybo.client.chatUnsubscribe(sessionId: sessionId)
    }

    // MARK: - Bridge lifecycle

    func attachBridge(_ bridge: TranscriptBridge) {
        self.bridge = bridge
        if needsHistoryReset {
            needsHistoryReset = false
            bufferedFrames.removeAll()
            // The buffer was dropped while offscreen; refetch the gap above the
            // rewound durable floor rather than flushing a hole-punched stream.
            // When not connected, the reconnect (`connectIfNeeded` on appear)
            // runs its own catch-up from the same cursor, so only fire here on a
            // live leg to avoid a redundant fetch.
            if connState == .connected {
                fetchCatchUp(sinceOrdinal: lastOrdinal ?? 0, generation: generation)
            }
            return
        }
        flushBufferedFrames(to: bridge)
    }

    func detachBridge(_ bridge: TranscriptBridge) {
        if self.bridge === bridge {
            self.bridge = nil
        }
    }

    private func pushFrame(_ frameJson: String) {
        // Already overflowed while offscreen: everything above the rewound floor
        // is refetched on the next attach, so don't buffer or advance the cursor
        // past the hole the dropped frames left.
        if bridge == nil && needsHistoryReset { return }
        advanceLastOrdinal(fromFrameJson: frameJson)
        if let bridge {
            bridge.pushFrame(frameJson)
        } else {
            bufferedFrames.append(frameJson)
            if bufferedFrames.count > Self.maxBufferedFrames {
                overflowBufferedFrames()
            }
        }
    }

    /// The offscreen buffer blew its cap (a long agent turn on a backgrounded
    /// session). Streamed deltas aren't durable rows, so a truncated flush would
    /// leave a hole — drop the buffer and rewind the catch-up cursor to the
    /// durable floor the webview actually holds, so the next `attachBridge`
    /// refetches the whole gap via catch-up/history instead.
    private func overflowBufferedFrames() {
        bufferedFrames.removeAll()
        needsHistoryReset = true
        lastOrdinal = Self.persistedOrdinal(sessionId: sessionId)
    }

    /// The newest ordinal in the durable transcript mirror — the safe catch-up
    /// floor (≤ what the webview has actually rendered). `nil` when no mirror
    /// exists yet. Read at init and again on a buffer-overflow reset, where the
    /// in-memory `lastOrdinal` has advanced past the frames that were dropped.
    private static func persistedOrdinal(sessionId: String) -> Int64? {
        guard let blob = TranscriptStore.read(sessionId: sessionId),
            let data = blob.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return nil }
        return (obj["lastOrdinal"] as? NSNumber)?.int64Value
    }

    private func advanceLastOrdinal(fromFrameJson frameJson: String) {
        guard let data = frameJson.data(using: .utf8),
            let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        if let ordinal = (frame["ordinal"] as? NSNumber)?.int64Value {
            noteDurableOrdinal(ordinal)
        }
        if let newest = (frame["newest_ordinal"] as? NSNumber)?.int64Value {
            noteDurableOrdinal(newest)
        }
    }

    private func noteDurableOrdinal(_ ordinal: Int64) {
        guard lastOrdinal == nil || ordinal > (lastOrdinal ?? 0) else { return }
        lastOrdinal = ordinal
    }

    private func flushBufferedFrames(to bridge: TranscriptBridge) {
        guard !bufferedFrames.isEmpty else { return }
        let frames = bufferedFrames
        bufferedFrames.removeAll()
        for frame in frames {
            bridge.pushFrame(frame)
        }
    }

    #if DEBUG
        func pushDemoUserSent(msgId: String, text: String) {
            bridge?.userSent(msgId: msgId, text: text, attachments: [])
        }

        func pushDemoFrame(_ frameJson: String) {
            pushFrame(frameJson)
        }
    #endif

    // MARK: - Sending

    /// Optimistic send: mint the idempotency key, seed the webview's bubble +
    /// echo-dedup FIRST, then enqueue on the live leg.
    func send(text: String, attachments: [AttachmentRef]) {
        let msgId = UUID().uuidString
        bridge?.userSent(msgId: msgId, text: text, attachments: attachments)
        dispatchSend(msgId: msgId, text: text, attachments: attachments)
    }

    /// Retry a send the webview flagged failed (its red dot tap). Reuses the
    /// original msgId as the idempotency key, so a resend that races a late
    /// first delivery still lands as a single row. The optimistic bubble already
    /// exists (the webview flipped it back to sending), so no fresh `userSent`.
    func retrySend(msgId: String, text: String, attachments: [AttachmentRef]) {
        dispatchSend(msgId: msgId, text: text, attachments: attachments)
    }

    /// Enqueue on the live leg (dialing first if needed); on any failure tell the
    /// webview to flag that bubble failed (`sendFailed`) — its red retry dot is
    /// the sole failure surface, no composer notice. `scheduleRetry` only redials
    /// the leg — it never re-enqueues this message — so the send is genuinely
    /// lost until the user taps retry.
    private func dispatchSend(msgId: String, text: String, attachments: [AttachmentRef]) {
        Task {
            do {
                try await ensureRemoteSession()
                SessionIndex.shared.recordUserSend(sessionId: sessionId, text: text)
                try await sendWhenReady(text: text, msgId: msgId, attachments: attachments)
            } catch {
                bridge?.sendFailed(msgId)
            }
        }
    }

    private func sendWhenReady(
        text: String,
        msgId: String,
        attachments: [AttachmentRef]
    ) async throws {
        if connState == .connected {
            try await Baybo.client.chatSend(
                sessionId: sessionId, text: text, msgId: msgId, attachments: attachments)
            return
        }

        retryTask?.cancel()
        retryTask = nil
        catchUpTask?.cancel()
        catchUpTask = nil
        connectTask?.cancel()
        connectTask = nil

        let sinceOrdinal = lastOrdinal
        connState = .connecting
        notice = nil
        issuedGeneration += 1
        let gen = issuedGeneration
        do {
            try await Baybo.client.chatSendAfterConnect(
                sessionId: sessionId,
                sinceOrdinal: sinceOrdinal,
                sink: Sink(store: self, generation: gen),
                text: text,
                msgId: msgId,
                attachments: attachments
            )
            guard gen >= generation else { return }
            generation = gen
            connEpoch += 1
            connState = .connected
            bridge?.setConnEpoch(connEpoch)
            if let sinceOrdinal {
                fetchCatchUp(sinceOrdinal: sinceOrdinal, generation: gen)
            }
        } catch {
            guard gen >= generation else { throw error }
            connState = .offline
            scheduleRetry()
            throw error
        }
    }

    private func ensureRemoteSession() async throws {
        if remoteSessionEnsured { return }
        if let task = ensureRemoteSessionTask {
            try await task.value
            return
        }

        let sessionId = sessionId
        let task = Task {
            _ = try await Baybo.client.chatCreateSession(sessionId: sessionId)
        }
        ensureRemoteSessionTask = task
        do {
            try await task.value
            remoteSessionEnsured = true
            ensureRemoteSessionTask = nil
        } catch {
            ensureRemoteSessionTask = nil
            throw error
        }
    }

    // MARK: - Bridge callbacks (webview → native)

    func ordinalAdvanced(_ ordinal: Int64?) {
        lastOrdinal = ordinal
    }

    func fetchHistory(beforeOrdinal: Int64?, limit: UInt32) {
        Task {
            do {
                let frame = try await Baybo.client.chatFetchHistory(
                    sessionId: sessionId, beforeOrdinal: beforeOrdinal, limit: limit)
                pushFrame(frame)
            } catch {
                NSLog("baybo: fetchHistory: %@", bayboErrorText(error))
                // The web bundle's paging/reset guards armed for this request
                // must unwind; a synthesized frame rides the ordered frame path
                // just like a successful native history fetch.
                let payload: [String: Any] = [
                    "kind": "history_failed",
                    "error": bayboErrorText(error),
                ]
                if let data = try? JSONSerialization.data(withJSONObject: payload),
                    let json = String(data: data, encoding: .utf8)
                {
                    pushFrame(json)
                }
            }
        }
    }

    func requestBlob(id: Int, blobId: String) {
        Task {
            do {
                let bytes = try await Baybo.client.blobDownloadBytes(blobId: blobId)
                bridge?.blobResult(
                    id: id, dataBase64: bytes.base64EncodedString(),
                    mimeType: sniffBlobMimeType(bytes), error: nil)
            } catch {
                bridge?.blobResult(
                    id: id, dataBase64: nil, mimeType: "", error: bayboErrorText(error))
            }
        }
    }

    /// Cheap magic-byte sniff so the webview can build a typed Blob; the exact
    /// subtype only matters for the object URL, so `image/*` fallbacks are fine.
    private func sniffBlobMimeType(_ data: Data) -> String {
        if data.starts(with: [0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if data.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "image/png" }
        if data.starts(with: [0x47, 0x49, 0x46]) { return "image/gif" }
        if data.count > 11, data[8...11] == Data([0x57, 0x45, 0x42, 0x50]) {
            return "image/webp"
        }
        return ""
    }

    /// The per-dial frame sink. Callbacks arrive on the core's tokio workers;
    /// hop to the main queue before touching state. GCD (not `Task`) on purpose:
    /// the main queue is FIFO, so streamed frames can't reorder — an
    /// `answer_delta` arriving before its predecessor would corrupt the
    /// transcript. Late callbacks from a superseded dial are dropped by the
    /// generation guard.
    ///
    /// `@unchecked Sendable`: the only mutable state is the auto-nilling weak
    /// ref (atomic), and all real work hops to the main queue.
    private final class Sink: FrameSink, @unchecked Sendable {
        private weak var store: ChatStore?
        private let generation: Int

        init(store: ChatStore, generation: Int) {
            self.store = store
            self.generation = generation
        }

        func onFrame(frameJson: String) {
            DispatchQueue.main.async { [weak store, generation] in
                MainActor.assumeIsolated {
                    guard let store, generation >= store.generation else { return }
                    store.pushFrame(frameJson)
                }
            }
        }

        func onDisconnected(sessionId: String) {
            DispatchQueue.main.async { [weak store, generation] in
                MainActor.assumeIsolated {
                    store?.pumpDisconnected(sessionId: sessionId, generation: generation)
                }
            }
        }
    }
}
