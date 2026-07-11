import AVFoundation
import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// A file attachment sitting in the app's temp dir, ready for QuickLook or the
/// share sheet. Identified by its URL so `.sheet(item:)` re-presents when the
/// user taps a different file.
struct FilePreview: Identifiable {
    let url: URL
    var id: URL { url }
}

/// A downloaded video the user tapped, materialised on disk and awaiting the
/// native full-screen player. Identified by blob id so `.fullScreenCover(item:)`
/// re-presents when a different video is tapped.
struct VideoPlayback: Identifiable {
    let id: String
    let url: URL
}

/// Bridges the core's `BlobProgress` callback (a tokio worker) onto the main
/// actor. The core already rate-limits the ticks.
final class BlobProgressForwarder: BlobProgress, @unchecked Sendable {
    private let onTick: @MainActor (UInt64, UInt64?) -> Void

    init(onTick: @escaping @MainActor (UInt64, UInt64?) -> Void) {
        self.onTick = onTick
    }

    func onProgress(downloaded: UInt64, total: UInt64?) {
        Task { @MainActor in onTick(downloaded, total) }
    }
}

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
    /// Full leading-slash form of the stop command (mirrors the gateway's
    /// `STOP_COMMAND`). Delivered as an ordinary chat send; the agent Router
    /// recognises it out-of-band and cancels the in-flight turn.
    static let stopCommand = "/stop"

    let sessionId: String
    /// Whether this session already exists on the gateway — an existing
    /// conversation (tapped from the list or a push), as opposed to a fresh
    /// compose draft. Gates connecting (a draft stays offline until its first
    /// send) and sync eligibility (a draft has nothing to pull). The webview no
    /// longer needs this: its sync loop runs the same on every open, and a
    /// draft's sync request is simply skipped native-side until the session
    /// exists.
    let listed: Bool
    @Published private(set) var connState: ConnState
    /// Transient composer notice (send failed / waiting for upload / too large).
    @Published var notice: String?
    /// A tapped file attachment, materialised on disk and awaiting presentation.
    @Published var filePreview: FilePreview?
    /// A tapped image attachment, decoded and awaiting the full-screen viewer.
    @Published var viewedImage: ViewedImage?
    /// A tapped (downloaded) video attachment, awaiting the full-screen player.
    @Published var videoPlayback: VideoPlayback?
    /// Whether a turn is in flight for this session — mirrored from the transcript
    /// webview (the single source of truth for turn state) over the bridge. Drives
    /// the composer's send↔stop button. Native never re-derives turn state; it
    /// only reflects what the webview reports.
    @Published private(set) var agentRunning = false

    /// Increments on every successful dial; the webview uses it to retry
    /// attachments that raced ahead of the leg going live, and — the sync-loop
    /// reconnect edge — to run one forward-recovery pull (`handleConnEpoch`).
    private(set) var connEpoch = 0
    private var remoteSessionEnsured: Bool
    private var ensureRemoteSessionTask: Task<Void, Error>?
    /// The persisted send outbox (survives relaunch alongside the transcript
    /// mirror). Confirmation is two-stage: the echo proves transport, an
    /// ordinal-stamped row (sync/backfill) proves durability. See sync-v2.
    private let outbox: OutboxStore

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
    private var connectTask: Task<Void, Error>?
    /// Fires the outbox retry sweep (10 s no-echo → one blind resend, up to the
    /// 3-transmission cap). Started lazily when the outbox is non-empty.
    private var outboxTimer: Task<Void, Never>?
    private weak var bridge: TranscriptBridge?
    private var bufferedFrames: [String] = []
    /// Set when the offscreen buffer overflowed and was dropped: the next
    /// `attachBridge` asks the webview to run its sync loop (from its own
    /// durable cursor) rather than flushing a hole-punched stream. Frames
    /// arriving meanwhile are dropped, not re-buffered.
    private var needsSyncOnAttach = false

    init(sessionId: String) {
        self.sessionId = sessionId
        let listed = SessionIndex.shared.contains(sessionId: sessionId)
        self.listed = listed
        connState = listed ? .connecting : .draft
        remoteSessionEnsured = false
        outbox = OutboxStore(sessionId: sessionId)
    }

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
                    sink: Sink(store: self, generation: gen)
                )
                guard gen >= generation else { return }  // superseded by disconnect
                // The subscribe is now accepted; only NOW mute older sinks. The
                // server replays no history — the connEpoch bump drives the
                // webview's sync loop (its `handleConnEpoch` → one forward pull).
                generation = gen
                connEpoch += 1
                connState = .connected
                bridge?.setConnEpoch(connEpoch)
                // Reconcile the outbox against the reconnect: entries still
                // lacking durability confirmation resend (sync-v2 send path).
                reconcileOutboxOnConnect()
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
        outboxTimer?.cancel()
        outboxTimer = nil
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
        outboxTimer?.cancel()
        outboxTimer = nil
        connectTask?.cancel()
        connectTask = nil
        issuedGeneration += 1
        generation = issuedGeneration
        await Baybo.client.chatUnsubscribe(sessionId: sessionId)
    }

    // MARK: - Bridge lifecycle

    func attachBridge(_ bridge: TranscriptBridge) {
        self.bridge = bridge
        defer { flushPendingAnswers(to: bridge) }
        if needsSyncOnAttach {
            needsSyncOnAttach = false
            bufferedFrames.removeAll()
            // The buffer was dropped while offscreen; the webview re-hydrates by
            // running its sync loop from its own durable cursor (which the
            // dropped live frames never advanced) rather than flushing a
            // hole-punched stream.
            bridge.requestSync()
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
        // Already overflowed while offscreen: the webview re-syncs on the next
        // attach, so don't buffer past the hole the dropped frames left.
        if bridge == nil && needsSyncOnAttach { return }
        // Passive observers on the same frame stream, parsed once: a user Echo
        // (ordinal-less, matching platform_msg_id) advances the outbox; the
        // turn's terminal assistant `message` updates the chat-list preview in
        // place. Both run even while offscreen (bridge nil, buffered) so a reply
        // that lands after the user backs out still updates the list live.
        if let data = frameJson.data(using: .utf8),
            let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        {
            outboxObserveFrame(frame)
            noteChatListPreview(frame)
        }
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
    /// leave a hole — drop the buffer and mark the session for a sync on the
    /// next `attachBridge`, where the webview pulls the whole gap from its own
    /// cursor.
    private func overflowBufferedFrames() {
        bufferedFrames.removeAll()
        needsSyncOnAttach = true
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

        func pushDemoFileState(
            blobId: String, state: String, loaded: UInt64? = nil, total: UInt64? = nil
        ) {
            bridge?.fileState(blobId: blobId, state: state, loaded: loaded, total: total)
        }

        func pushDemoBlobResult(id: Int, dataBase64: String, mimeType: String) {
            bridge?.blobResult(id: id, dataBase64: dataBase64, mimeType: mimeType, error: nil)
        }

        func pushDemoVideoPoster(
            id: Int, dataBase64: String, width: Int, height: Int, durationMs: Int
        ) {
            bridge?.videoPoster(
                id: id, dataBase64: dataBase64, width: width, height: height,
                durationMs: durationMs)
        }
    #endif

    // MARK: - Sending

    /// Optimistic send: mint the idempotency key, seed the webview's bubble +
    /// echo-dedup FIRST, enrol the persisted outbox entry, then enqueue on the
    /// live leg.
    func send(text: String, attachments: [AttachmentRef]) {
        let msgId = UUID().uuidString
        bridge?.userSent(msgId: msgId, text: text, attachments: attachments)
        outbox.beginSend(
            platformMsgId: msgId, text: text, attachments: attachments.map(Self.toOutboxAttachment))
        startOutboxTimerIfNeeded()
        dispatchSend(msgId: msgId, text: text, attachments: attachments)
    }

    /// Retry a send the webview flagged failed (its red dot tap). Reuses the
    /// original msgId as the idempotency key, so a resend that races a late
    /// first delivery still lands as a single row. The optimistic bubble already
    /// exists (the webview flipped it back to sending), so no fresh `userSent`.
    /// The manual retry resets the automatic-transmission budget.
    func retrySend(msgId: String, text: String, attachments: [AttachmentRef]) {
        outbox.resetForManualRetry(
            platformMsgId: msgId, text: text, attachments: attachments.map(Self.toOutboxAttachment))
        startOutboxTimerIfNeeded()
        dispatchSend(msgId: msgId, text: text, attachments: attachments)
    }

    /// Enqueue on the live leg (dialing first if needed); on any transport
    /// failure tell the webview to flag that bubble failed (`sendFailed`) — its
    /// red retry dot is the manual failure surface. The persisted outbox drives
    /// automatic retry (10 s no-echo → one blind resend, capped at 3
    /// transmissions) and resend-after-sync on reconnect.
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
        connectTask?.cancel()
        connectTask = nil

        connState = .connecting
        notice = nil
        issuedGeneration += 1
        let gen = issuedGeneration
        do {
            try await Baybo.client.chatSendAfterConnect(
                sessionId: sessionId,
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
            reconcileOutboxOnConnect()
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

    /// The one forward-recovery pull. The webview posts its cursor (`nil` =
    /// baseline); native fetches `GET …/sync?since_ordinal=…&limit=…` over the
    /// active leg, reconciles the outbox against the returned rows, then pushes
    /// the `sync_page` frame back for the webview to apply.
    func requestSync(sinceOrdinal: Int64?, limit: UInt32) {
        guard listed || remoteSessionEnsured else {
            // A draft has nothing to pull yet — but the webview already flipped
            // its in-flight guard, so we MUST reply. An empty baseline
            // `sync_page` clears it (an empty REPLACE keeps the optimistic
            // bubble intact); otherwise the guard would stay set forever and
            // the connEpoch sync after the first send would be a no-op until
            // reload.
            pushSynthesizedFrame([
                "kind": "sync_page",
                "rows": [],
                "since_ordinal": sinceOrdinal.map { NSNumber(value: $0) } ?? NSNull(),
                "next_cursor": NSNull(),
                "rebased": false,
                "oldest_ordinal": NSNull(),
                "has_more_older": false,
            ])
            return
        }
        Task {
            do {
                let frame = try await Baybo.client.chatFetchSync(
                    sessionId: sessionId, sinceOrdinal: sinceOrdinal, limit: limit)
                reconcileOutboxAfterSync(frameJson: frame)
                pushFrame(frame)
            } catch {
                NSLog("baybo: sync: %@", bayboErrorText(error))
                // Unwind the webview's in-flight sync guard so the next trigger
                // retries; the durable record is intact server-side.
                pushSynthesizedFrame(["kind": "sync_failed", "error": bayboErrorText(error)])
            }
        }
    }

    /// Advance the server chat-list read cursor (max-wins) to `ordinal` — the
    /// viewer has read up to here. Fire-and-forget: the badge clears on the next
    /// list pull; a draft (no remote session yet) has nothing to mark.
    func markRead(ordinal: Int64) {
        guard listed || remoteSessionEnsured else { return }
        Task {
            do {
                try await Baybo.client.chatMarkRead(sessionId: sessionId, ordinal: ordinal)
            } catch {
                NSLog("baybo: mark read: %@", bayboErrorText(error))
            }
        }
    }

    /// The transcript reported whether a turn is in flight (from its turn state
    /// plus the optimistic post-send window). Deduped so an unchanged report
    /// doesn't publish a redundant `objectWillChange`.
    func setAgentRunning(_ running: Bool) {
        if agentRunning != running { agentRunning = running }
    }

    /// Cancel the session's in-flight turn: deliver `/stop` over the live leg.
    /// The gateway's agent Router intercepts it out-of-band (before routing a
    /// turn), so it never becomes a user message / bubble; it cancels the turn
    /// and every subagent it spawned. Fire-and-forget and deliberately lighter
    /// than `send` — no optimistic bubble, no outbox enrolment: a stop that
    /// fails leaves the turn running and the user can tap again. When the leg is
    /// mid-reconnect (a turn keeps running server-side while `connState` briefly
    /// leaves `.connected`) we don't silently drop the tap — kick a redial so the
    /// leg comes back and re-syncs, and the user can stop again once connected.
    func stopAgent() {
        guard connState == .connected else {
            if connState != .draft { connect() }
            return
        }
        let msgId = UUID().uuidString
        Task {
            do {
                try await Baybo.client.chatSend(
                    sessionId: sessionId, text: Self.stopCommand, msgId: msgId, attachments: [])
            } catch {
                NSLog("baybo: stop: %@", bayboErrorText(error))
            }
        }
    }

    func fetchHistory(beforeOrdinal: Int64?, limit: UInt32) {
        Task {
            do {
                let frame = try await Baybo.client.chatFetchHistory(
                    sessionId: sessionId, beforeOrdinal: beforeOrdinal, limit: limit)
                pushFrame(frame)
            } catch {
                NSLog("baybo: fetchHistory: %@", bayboErrorText(error))
                // The web bundle's paging guards armed for this request must
                // unwind; a synthesized frame rides the ordered frame path just
                // like a successful native history fetch.
                pushSynthesizedFrame(["kind": "history_failed", "error": bayboErrorText(error)])
            }
        }
    }

    private func pushSynthesizedFrame(_ payload: [String: Any]) {
        if let data = try? JSONSerialization.data(withJSONObject: payload),
            let json = String(data: data, encoding: .utf8)
        {
            pushFrame(json)
        }
    }

    // MARK: - Outbox (persisted send reconciliation)

    /// A frame flowing to the webview: a user Echo (role `user`, a
    /// `platform_msg_id`, no `ordinal`) proves transport, flipping its outbox
    /// entry to `sent`. Durability release happens on a sync/backfill row
    /// instead (`reconcileOutboxAfterSync`). `frame` is the caller's one-time
    /// parse of the JSON, shared with `noteChatListPreview`.
    private func outboxObserveFrame(_ frame: [String: Any]) {
        guard (frame["kind"] as? String) == "message",
            (frame["role"] as? String) == "user",
            let pmid = frame["platform_msg_id"] as? String, !pmid.isEmpty,
            frame["ordinal"] == nil || frame["ordinal"] is NSNull
        else { return }
        outbox.markEchoed(platformMsgId: pmid)
    }

    /// The turn's terminal assistant `message` is the authoritative, tool-free
    /// final answer (the web bundle treats it as such — it supersedes the
    /// stream and closes the work block). Capture its text as the chat-list
    /// row's preview so the list's second line updates in place while the chat
    /// is open (or still resident after backing out), sparing the user the
    /// user-message→reply jump the return refresh would otherwise show.
    private func noteChatListPreview(_ frame: [String: Any]) {
        guard (frame["kind"] as? String) == "message",
            (frame["role"] as? String) == "assistant",
            let content = frame["content"] as? String
        else { return }
        SessionIndex.shared.recordAgentReply(sessionId: sessionId, text: content)
    }

    /// Native holds the `sync_page` it fetched, so it reconciles the outbox
    /// against it before the webview applies it: an ordinal-stamped row with a
    /// matching `platform_msg_id` proves DURABILITY and releases the entry. A
    /// rebased page hides the floor, so each still-unconfirmed entry resolves via
    /// the per-key point lookup (no retry transmission consumed).
    private func reconcileOutboxAfterSync(frameJson: String) {
        guard let data = frameJson.data(using: .utf8),
            let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        let rows = (frame["rows"] as? [[String: Any]]) ?? []
        for row in rows {
            if let pmid = row["platform_msg_id"] as? String, !pmid.isEmpty {
                outbox.confirmDurable(platformMsgId: pmid)
            }
        }
        if (frame["rebased"] as? Bool) == true {
            for entry in outbox.entries() where entry.state != .failed {
                resolveUnknownEntry(platformMsgId: entry.platformMsgId)
            }
        }
    }

    /// After a rebased sync hid the floor: probe the key's durability directly.
    /// Found → release; provably absent → the retry machine resumes.
    private func resolveUnknownEntry(platformMsgId: String) {
        outbox.markUnknown(platformMsgId: platformMsgId)
        Task {
            do {
                let lookup = try await Baybo.client.chatLookupMessage(
                    sessionId: sessionId, platformMsgId: platformMsgId)
                if lookup.found {
                    outbox.confirmDurable(platformMsgId: platformMsgId)
                } else {
                    outbox.resumeSending(platformMsgId: platformMsgId)
                    startOutboxTimerIfNeeded()
                }
            } catch {
                NSLog("baybo: point lookup: %@", bayboErrorText(error))
            }
        }
    }

    /// Reconnect edge: resend every outbox entry still lacking durability
    /// confirmation (the loop's sync runs first as the reconciliation gate — the
    /// webview drives it off the connEpoch bump). The gateway-restart crash
    /// window (echoed, never persisted) is exactly what this recovers.
    private func reconcileOutboxOnConnect() {
        // Reconciliation gate: resolve each unconfirmed entry by a per-key
        // durability point lookup BEFORE any resend. A blind resend here would
        // double-run a send that IS durable when the gateway restarted and
        // evicted its in-memory dedup — the lookup (served from the persisted
        // column, which survives a restart) confirms it instead. Found →
        // release; provably absent → the retry timer resends (or fails at the
        // cap); a lookup error defers to the next reconnect (never a blind
        // resend). This is the iOS analogue of the web loop's "sync first".
        for entry in outbox.entries() {
            guard entry.state == .sending || entry.state == .sent else { continue }
            resolveUnknownEntry(platformMsgId: entry.platformMsgId)
        }
        startOutboxTimerIfNeeded()
    }

    /// One (re)transmission of an outbox entry on the live leg (same
    /// `platform_msg_id`, safe inside the gateway's dedup window). Replays the
    /// entry's attachments too, so a retried image/file send stays that message
    /// rather than degrading to text-only.
    private func resendOutboxEntry(_ entry: OutboxEntry) {
        guard connState == .connected else { return }
        outbox.recordTransmission(platformMsgId: entry.platformMsgId)
        let attachments = entry.attachments.map(Self.toAttachmentRef)
        Task {
            do {
                try await Baybo.client.chatSend(
                    sessionId: sessionId, text: entry.text, msgId: entry.platformMsgId,
                    attachments: attachments)
            } catch {
                NSLog("baybo: outbox resend: %@", bayboErrorText(error))
            }
        }
    }

    private nonisolated static func toOutboxAttachment(_ ref: AttachmentRef) -> OutboxAttachment {
        let kind: String
        switch ref.kind {
        case .image: kind = "image"
        case .audio: kind = "audio"
        case .file: kind = "file"
        }
        return OutboxAttachment(
            kind: kind, blobId: ref.blobId, mimeType: ref.mimeType, size: ref.size,
            filename: ref.filename)
    }

    private nonisolated static func toAttachmentRef(_ att: OutboxAttachment) -> AttachmentRef {
        let kind: AttachmentKind
        switch att.kind {
        case "image": kind = .image
        case "audio": kind = .audio
        default: kind = .file
        }
        return AttachmentRef(
            kind: kind, blobId: att.blobId, mimeType: att.mimeType, size: att.size,
            filename: att.filename)
    }

    private func failOutboxEntry(_ platformMsgId: String) {
        outbox.markFailed(platformMsgId: platformMsgId)
        bridge?.sendFailed(platformMsgId)
    }

    /// The retry sweep: while there are `sending`/`sent` entries, every ~3 s
    /// resend one whose 10 s echo window lapsed, or fail one that exhausted the
    /// 3-transmission cap. Self-stops when the outbox drains. The `Task` inherits
    /// this store's `@MainActor` isolation, so the sweep touches state directly.
    private func startOutboxTimerIfNeeded() {
        guard outboxTimer == nil else { return }
        outboxTimer = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: OutboxStore.retryTick)
                guard let self else { return }
                if self.sweepOutbox() { return }
            }
        }
    }

    /// One retry-sweep pass. Returns `true` when the outbox is idle (no
    /// `sending`/`sent` entries left) so the timer can stop.
    private func sweepOutbox() -> Bool {
        var active = false
        for entry in outbox.entries() {
            guard entry.state == .sending || entry.state == .sent else { continue }
            active = true
            if entry.state != .sending { continue }
            if outbox.dueForBlindResend(entry) {
                resendOutboxEntry(entry)
            } else if outbox.resendExhausted(entry) {
                failOutboxEntry(entry.platformMsgId)
            }
        }
        if !active {
            outboxTimer = nil
            return true
        }
        return false
    }

    func requestBlob(id: Int, blobId: String) {
        #if DEBUG
            // `-baybo-demo-images` serves its own bytes — an image is the one
            // attachment kind that can't be faked from a frame alone.
            if serveDemoImageIfRequested(id: id, blobId: blobId) { return }
        #endif
        Task {
            do {
                // A thumbnail fetch: nobody is watching the byte count, and the
                // core skips the tick machinery entirely for a nil observer.
                let bytes = try await Baybo.client.blobDownloadBytes(
                    blobId: blobId, progress: nil)
                // Encode off the main actor: base64 of a large blob (up to
                // 100 MiB) would stall every tap for seconds.
                let (encoded, mime) = await Task.detached(priority: .userInitiated) {
                    (bytes.base64EncodedString(), Self.sniffBlobMimeType(bytes))
                }.value
                bridge?.blobResult(id: id, dataBase64: encoded, mimeType: mime, error: nil)
            } catch {
                bridge?.blobResult(
                    id: id, dataBase64: nil, mimeType: "", error: bayboErrorText(error))
            }
        }
    }

    // MARK: - File attachments (download → preview)

    /// Blobs with a download task in flight, so a second tap joins rather than
    /// racing a duplicate stream through the core's per-blob cache lock.
    private var fileDownloads: Set<String> = []

    private struct PendingFileState {
        let state: String
        let loaded: UInt64?
        let total: UInt64?
        let error: String?
    }

    /// Bridge ANSWERS that landed while no webview was attached. Wire frames
    /// buffer (`bufferedFrames`); these used to just drop — and a download
    /// whose terminal `ready` fell in the detach window wedged its card at
    /// `loading` forever, because a same-session re-attach remounts nothing and
    /// so re-queries nothing. Last-write-wins per blob; posters keep every
    /// reply (ids are one-shot promises). Flushed on `attachBridge`; a reply
    /// whose session switched away settles nothing web-side (`init` cleared the
    /// pending map) and is ignored there.
    private var pendingFileStates: [String: PendingFileState] = [:]
    private var pendingPosterReplies: [PendingPosterReply] = []

    private struct PendingPosterReply {
        let id: Int
        let dataBase64: String?
        let width: Int
        let height: Int
        let durationMs: Int
        let error: String?
    }

    private func pushFileState(
        blobId: String, state: String, loaded: UInt64? = nil, total: UInt64? = nil,
        error: String? = nil
    ) {
        if let bridge {
            bridge.fileState(
                blobId: blobId, state: state, loaded: loaded, total: total, error: error)
        } else {
            pendingFileStates[blobId] = PendingFileState(
                state: state, loaded: loaded, total: total, error: error)
        }
    }

    private func pushVideoPoster(
        id: Int, dataBase64: String?, width: Int, height: Int, durationMs: Int,
        error: String? = nil
    ) {
        if let bridge {
            bridge.videoPoster(
                id: id, dataBase64: dataBase64, width: width, height: height,
                durationMs: durationMs, error: error)
        } else {
            pendingPosterReplies.append(
                PendingPosterReply(
                    id: id, dataBase64: dataBase64, width: width, height: height,
                    durationMs: durationMs, error: error))
        }
    }

    private func flushPendingAnswers(to bridge: TranscriptBridge) {
        let states = pendingFileStates
        pendingFileStates.removeAll()
        for (blobId, s) in states {
            bridge.fileState(
                blobId: blobId, state: s.state, loaded: s.loaded, total: s.total, error: s.error)
        }
        let posters = pendingPosterReplies
        pendingPosterReplies.removeAll()
        for p in posters {
            bridge.videoPoster(
                id: p.id, dataBase64: p.dataBase64, width: p.width, height: p.height,
                durationMs: p.durationMs, error: p.error)
        }
    }

    /// Answer the card's mount-time probe. The blob cache lives in the OS temp
    /// dir, so this is asked every mount rather than remembered.
    func queryFileState(blobId: String) {
        if fileDownloads.contains(blobId) {
            pushFileState(blobId: blobId, state: "loading")
            return
        }
        Task {
            let cached = await Baybo.client.blobIsCached(blobId: blobId)
            pushFileState(blobId: blobId, state: cached ? "ready" : "idle")
        }
    }

    func downloadFile(blobId: String) {
        guard fileDownloads.insert(blobId).inserted else { return }
        pushFileState(blobId: blobId, state: "loading", loaded: 0)
        Task {
            defer { fileDownloads.remove(blobId) }
            do {
                _ = try await Baybo.client.blobDownloadBytes(
                    blobId: blobId,
                    progress: BlobProgressForwarder { [weak self] loaded, total in
                        self?.pushFileState(
                            blobId: blobId, state: "loading", loaded: loaded, total: total)
                    })
                pushFileState(blobId: blobId, state: "ready")
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// Materialise the blob under its real name — QuickLook and the share sheet
    /// both pick the handler from the extension, and the core's cache names its
    /// files by digest — then hand it to the screen.
    func previewFile(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let bytes = try await Baybo.client.blobDownloadBytes(
                    blobId: blobId, progress: nil)
                let url = try Self.writePreviewFile(
                    bytes: bytes, blobId: blobId, filename: filename, mimeType: mimeType)
                filePreview = FilePreview(url: url)
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// Open a tapped image full-screen. The blob is device-cached (the thumbnail
    /// fetch wrote it), so this decodes near-instantly; a non-decodable blob
    /// simply doesn't present. Its own viewer rather than QuickLook so pinch-zoom,
    /// double-tap-to-restore, and the black chat-image field are guaranteed.
    func viewImage(blobId: String, filename: String, mimeType: String) {
        Task {
            guard let bytes = try? await Baybo.client.blobDownloadBytes(
                blobId: blobId, progress: nil),
                let image = UIImage(data: bytes)
            else { return }
            // The share sheet hands over the FILE, not the decoded image, so the
            // original encoding and name reach Photos / Files / AirDrop. A write
            // failure only costs the share button, never the viewer.
            let url = try? Self.writePreviewFile(
                bytes: bytes, blobId: blobId, filename: filename, mimeType: mimeType)
            viewedImage = ViewedImage(id: blobId, image: image, url: url)
        }
    }

    // MARK: - Audio + video attachments

    /// Play/pause an audio card. The card only posts this once the blob is on
    /// disk, so the byte read is a cache hit; materialising under the real name
    /// gives AVPlayer an extension to sniff the container by.
    func audioToggle(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                AudioPlayerCenter.shared.toggle(
                    blobId: blobId, url: url, title: filename, bridge: bridge)
            } catch {
                // "failed" over "ready" on purpose: the worst failure mode here
                // is the cache getting purged between the probe and the tap
                // with the refetch failing — the blob is genuinely gone, and
                // failed's tap-to-redownload is the honest affordance.
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    func audioSeek(blobId: String, position: Double) {
        AudioPlayerCenter.shared.seek(blobId: blobId, position: position, bridge: bridge)
    }

    func queryAudioState(blobId: String) {
        AudioPlayerCenter.shared.queryState(blobId: blobId, bridge: bridge)
    }

    /// Hand a downloaded video to the native full-screen player. Chat audio
    /// yields first — two engines over one AVAudioSession just fight.
    func playVideo(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                // The user backed out while the file materialised: presenting
                // now would arm a stale fullScreenCover for the NEXT entry and
                // kill audio they started elsewhere. The bridge doubles as the
                // on-screen token (detached in ChatScreen.onDisappear).
                guard bridge != nil else { return }
                AudioPlayerCenter.shared.stop()
                videoPlayback = VideoPlayback(id: blobId, url: url)
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// A video card asking for its poster: first frame + natural size +
    /// duration, generated off the materialised file (AVAssetImageGenerator
    /// needs a pathed asset, and the extension picks the demuxer).
    func requestVideoPoster(id: Int, blobId: String, filename: String, mimeType: String) {
        #if DEBUG
            if serveDemoVideoPosterIfRequested(id: id, blobId: blobId) { return }
        #endif
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                let poster = try await Self.loadOrGeneratePoster(for: url)
                pushVideoPoster(
                    id: id,
                    dataBase64: poster.jpeg.base64EncodedString(),
                    width: poster.width,
                    height: poster.height,
                    durationMs: poster.durationMs)
            } catch {
                pushVideoPoster(
                    id: id, dataBase64: nil, width: 0, height: 0, durationMs: 0,
                    error: bayboErrorText(error))
            }
        }
    }

    private struct GeneratedPoster {
        let jpeg: Data
        let width: Int
        let height: Int
        let durationMs: Int
    }

    private struct PosterMeta: Codable {
        let width: Int
        let height: Int
        let durationMs: Int
    }

    private struct PosterEncodeError: Error, LocalizedError {
        var errorDescription: String? { "poster frame could not be encoded" }
    }

    /// Poster cache beside the materialised file (`poster.jpg` + `poster.json`
    /// in the digest dir): the tile re-requests its poster on EVERY remount
    /// (session switch, relaunch), and AVAssetImageGenerator + JPEG encode are
    /// too heavy to re-run each time. tmp-resident like the preview file — a
    /// purge just regenerates.
    private nonisolated static func loadOrGeneratePoster(for url: URL) async throws
        -> GeneratedPoster
    {
        let dir = url.deletingLastPathComponent()
        let jpegURL = dir.appendingPathComponent("poster.jpg")
        let metaURL = dir.appendingPathComponent("poster.json")
        if let jpeg = try? Data(contentsOf: jpegURL),
            let metaData = try? Data(contentsOf: metaURL),
            let meta = try? JSONDecoder().decode(PosterMeta.self, from: metaData)
        {
            return GeneratedPoster(
                jpeg: jpeg, width: meta.width, height: meta.height,
                durationMs: meta.durationMs)
        }
        let poster = try await generateVideoPoster(url: url)
        let meta = PosterMeta(
            width: poster.width, height: poster.height, durationMs: poster.durationMs)
        try? poster.jpeg.write(to: jpegURL, options: .atomic)
        try? JSONEncoder().encode(meta).write(to: metaURL, options: .atomic)
        return poster
    }

    /// First frame, downscaled — the tile is ~19rem wide; a full 4K frame
    /// base64'd over the bridge would be pure waste.
    private nonisolated static func generateVideoPoster(url: URL) async throws -> GeneratedPoster {
        let asset = AVURLAsset(url: url)
        let generator = AVAssetImageGenerator(asset: asset)
        generator.appliesPreferredTrackTransform = true
        generator.maximumSize = CGSize(width: 1024, height: 1024)
        let duration = try await asset.load(.duration)
        let (cgImage, _) = try await generator.image(at: CMTime(value: 0, timescale: 600))
        guard let jpeg = UIImage(cgImage: cgImage).jpegData(compressionQuality: 0.72) else {
            throw PosterEncodeError()
        }
        return GeneratedPoster(
            jpeg: jpeg,
            width: cgImage.width,
            height: cgImage.height,
            durationMs: duration.isNumeric ? Int(duration.seconds * 1000) : 0)
    }

    /// In-flight materialisations by target path: a poster request and a play
    /// tap for the same blob share one byte round-trip instead of holding the
    /// whole video in memory twice.
    private var previewMaterializations: [URL: Task<URL, Error>] = [:]

    /// The preview-file path without re-reading the blob when the named file is
    /// already on disk — poster generation and playback ask repeatedly for the
    /// same materialisation.
    private func materializePreviewFile(
        blobId: String, filename: String, mimeType: String
    ) async throws -> URL {
        let url = try Self.previewFileURL(blobId: blobId, filename: filename, mimeType: mimeType)
        if FileManager.default.fileExists(atPath: url.path) { return url }
        if let inFlight = previewMaterializations[url] {
            return try await inFlight.value
        }
        let task = Task {
            let bytes = try await Baybo.client.blobDownloadBytes(blobId: blobId, progress: nil)
            try bytes.write(to: url, options: .atomic)
            return url
        }
        previewMaterializations[url] = task
        defer { previewMaterializations[url] = nil }
        return try await task.value
    }

    /// `<tmp>/baybo-preview/<blob digest>/<filename>` — the digest directory
    /// keeps two blobs that share a filename apart, and lets a re-open reuse the
    /// file already written.
    private static func writePreviewFile(
        bytes: Data, blobId: String, filename: String, mimeType: String
    ) throws -> URL {
        let url = try previewFileURL(blobId: blobId, filename: filename, mimeType: mimeType)
        if !FileManager.default.fileExists(atPath: url.path) {
            try bytes.write(to: url, options: .atomic)
        }
        return url
    }

    private static func previewFileURL(
        blobId: String, filename: String, mimeType: String
    ) throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("baybo-preview", isDirectory: true)
            .appendingPathComponent(previewDirComponent(for: blobId), isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent(previewFilename(filename, mimeType: mimeType))
    }

    /// The digest half of `sha256:<hex>.<read-token>`; the token is a capability
    /// and rotates per upload, so it must never key a directory.
    private static func previewDirComponent(for blobId: String) -> String {
        let hex = blobId.drop(while: { $0 != ":" }).dropFirst().prefix(while: { $0 != "." })
        return hex.isEmpty ? "blob" : String(hex)
    }

    /// A nameless blob still needs an extension or QuickLook can't pick a
    /// previewer; derive one from the mime.
    private static func previewFilename(_ filename: String, mimeType: String) -> String {
        let trimmed = filename.replacingOccurrences(of: "/", with: "_")
        if !trimmed.isEmpty, trimmed.contains(".") { return trimmed }
        let ext = UTType(mimeType: mimeType)?.preferredFilenameExtension
        let base = trimmed.isEmpty ? "attachment" : trimmed
        return ext.map { "\(base).\($0)" } ?? base
    }

    /// Cheap magic-byte sniff so the webview can build a typed Blob; the exact
    /// subtype only matters for the object URL, so `image/*` fallbacks are fine.
    private nonisolated static func sniffBlobMimeType(_ data: Data) -> String {
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
