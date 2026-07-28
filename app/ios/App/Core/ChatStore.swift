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
    nonisolated static let maxAttachmentBytes = 100 * 1024 * 1024
    /// How many picks the composer will stage on ONE message. A UI limit, not
    /// a second copy of a wire cap: multi-select makes an accidental 200-file
    /// pick one gesture away, and every staged item holds an upload, a
    /// thumbnail and a strip tile. The gateway enforces its own per-message
    /// attachment cap independently.
    static let maxStagedAttachments = 10
    /// How many staged picks upload at once; the rest queue. A ten-pick batch
    /// fired off in parallel is ten sockets on one uplink AND ten 100ms
    /// progress tickers hopping to the main actor — about a hundred composer
    /// re-evaluations a second, which defeats the coalescing the tick interval
    /// exists to provide.
    static let maxConcurrentUploads = 2
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
    /// One-shot composer seed for a fresh draft (the Deck "Quick setup"): on
    /// appear `ComposerView` copies it into the field and sends it, then
    /// clears this; nil for every normal open. Not `@Published` — it is read
    /// once at mount, never observed.
    var initialDraft: String?
    @Published private(set) var connState: ConnState {
        didSet { watchLegDown() }
    }
    /// Sustained-disconnect signal behind the header's offline icon. Raw
    /// `connState` can't drive it: a real outage OSCILLATES `connecting ↔
    /// offline` through the retry loop — `offline` is only the 2s gap between
    /// failed dials, and a dead network's dial can hang in `connecting` for
    /// its whole timeout — so an `offline`-gated indicator barely ever showed.
    /// This flips true once the leg has been away from `.connected` for
    /// `legDownDelay` (drafts have no leg to be down), and clears the moment
    /// a dial lands.
    @Published private(set) var legDown = false
    /// A real outage wants seconds of debounce (entry/foreground dials pass
    /// through `.connecting` on every healthy open); a test wants ms.
    var legDownDelay: Duration = .seconds(4)
    private var legDownTask: Task<Void, Never>?
    /// Transient composer notice (send failed / waiting for upload / too large).
    /// Its lifetime is the VISIT, not the writer's, and that is enforced on the
    /// WRITE: a raise is dropped once `chatOpen` is false. Retracting always
    /// lands — taking a line back can never strand one.
    ///
    /// `leaveChat`'s retraction alone could not hold this. Every raise but the
    /// strip's comes from an unstructured `Task` that nothing cancels on leave —
    /// an approval echo that times out seconds later, the pin line a send defers
    /// to its tail, a queued model PUT — so each could land AFTER the retraction
    /// ran; and this store stays cached resident (`AppStore`'s LRU), so the line
    /// then stood in red over the composer on the next visit to the
    /// conversation.
    var notice: String? {
        get { noticeLine }
        set {
            guard chatOpen || newValue == nil else { return }
            noticeLine = newValue
        }
    }
    @Published private var noticeLine: String?
    /// Whether this conversation is the one the user is in — the property every
    /// notice write consults. A store exists because a conversation is being
    /// opened, so it starts true; `leaveChat` ends the visit; `attachBridge`
    /// starts the next one (the single transcript webview is aimed at exactly
    /// the conversation on screen, and `ChatScreen.onAppear` re-aims it through
    /// `TranscriptBridge.retarget` on every entry).
    private(set) var chatOpen = true
    /// The composer's staged attachment strip. Owned by the SESSION, not by the
    /// composer frame: `ChatScreen` docks the composer in a `.safeAreaInset`,
    /// and every `fullScreenCover` it presents — the image viewer, the video
    /// player — tears that inset down and puts it back. A strip whose lifetime
    /// was the frame's therefore lost mid-upload picks to a tap on a transcript
    /// image. What "the user actually left" means is `AppStore.chatPath`
    /// (`leaveChat` below), exactly as it is for audio.
    private(set) lazy var staging = ComposerStaging(store: self, client: client)
    /// A tapped file attachment, materialised on disk and awaiting presentation.
    @Published var filePreview: FilePreview?
    /// A long-pressed attachment awaiting the system share sheet.
    @Published var fileShare: FilePreview?
    /// A tapped image attachment, decoded and awaiting the full-screen viewer.
    @Published var viewedImage: ViewedImage?
    /// A tapped (downloaded) video attachment, awaiting the full-screen player.
    @Published var videoPlayback: VideoPlayback?
    /// Whether a turn is in flight for this session — mirrored from the transcript
    /// webview (the single source of truth for turn state) over the bridge. Drives
    /// the composer's send↔stop button. Native never re-derives turn state; it
    /// only reflects what the webview reports.
    @Published private(set) var agentRunning = false
    /// Tool calls blocked on this user's decision, oldest first — the composer's
    /// approval card renders the head and counts the rest. Derived NATIVELY from
    /// the frame stream (`pushFrame`), not mirrored from the webview: the
    /// offscreen buffer can overflow and drop frames, and the sync loop restores
    /// rows but not pending prompts, so a web-held queue could lose a card for
    /// good. The card is the only way to answer, and an unanswered gate denies
    /// after 5 minutes. The rendering mirror of `approvals`, republished only on
    /// a real edit.
    @Published private(set) var pendingApprovals: [PendingApproval] = []
    /// The session's model pin (`last_llm`): the LLM entry name its turns
    /// resolve against, or `nil` to follow the gateway's `default-llm`. Drives
    /// the header pill + picker checkmarks. Written optimistically by
    /// `selectModel`, seeded from the session meta by `refreshModelPin`.
    @Published private(set) var modelPin: String?
    /// The model chosen WITHIN `modelPin`'s entry (a `model_candidates` id),
    /// or `nil` for the entry's default model. Paired with `modelPin`
    /// everywhere; the pill shows this.
    @Published private(set) var modelPinModel: String?
    /// The session's reasoning-effort pin (`last_effort`), or `nil` for the
    /// entry's default effort. Independent of the llm/model pin (applies
    /// whatever entry runs); drives the header's thinking-level checkmark.
    @Published private(set) var modelPinEffort: String?
    /// Whether the pin is trustworthy: a seed fetch succeeded, or the user
    /// picked locally. Until then the pill shows a neutral placeholder — a
    /// listed session whose pin read FAILED (offline open) may well be pinned,
    /// and confidently labelling it with the default entry would misreport
    /// what the next turn runs. A draft starts resolved: it has no remote row,
    /// so its nil pin is authoritative.
    @Published private(set) var modelPinResolved: Bool

    /// Increments on every successful dial; the webview uses it to retry
    /// attachments that raced ahead of the leg going live, and — the sync-loop
    /// reconnect edge — to run one forward-recovery pull (`handleConnEpoch`).
    private(set) var connEpoch = 0
    /// The Rust core. Injected (UniFFI generates `BayboClientProtocol` and
    /// `BayboClient` conforms) so the send/sync/approval machinery can be driven
    /// without a gateway, a tokio runtime, or a keychain.
    private let client: any BayboClientProtocol
    /// The device-local chat-list registry this store records sends/replies into.
    private let index: SessionIndex
    private var remoteSessionEnsured: Bool
    private var ensureRemoteSessionTask: Task<Void, Error>?
    /// The persisted send outbox (survives relaunch alongside the transcript
    /// mirror). Confirmation is two-stage: the echo proves transport, an
    /// ordinal-stamped row (sync/backfill) proves durability. See sync-v2.
    private let outbox: OutboxStore
    /// The pending-approval state machine; `pendingApprovals` is its published
    /// mirror.
    private var approvals = ApprovalQueue()
    /// Monotonic guard for model-pin traffic: a slow `refreshModelPin` fetch or
    /// an out-of-order PUT failure must never clobber a newer selection.
    private var modelPinEpoch = 0
    /// Coalesces the per-open pin reads; deliberately NOT a once-per-store
    /// latch. This store is cached resident by `AppStore` (LRU), the gateway
    /// broadcasts no frame when another client re-pins the session, and the
    /// open-edge read is therefore the ONLY sync point — latching it off left
    /// the pill stale forever on exactly the most-used sessions.
    private var modelPinFetchInFlight = false
    /// A selection made while the session was still a draft (no remote row to
    /// pin), stamped with the epoch of that pick: a newer selection supersedes
    /// it and the stale PUT is skipped outright — re-sending it could land
    /// AFTER the newer pick's PUT (writes have no wire order) and flip the
    /// gateway back. `entry: nil` means "picked the default back": a draft's
    /// remote pin doesn't exist yet, so there is nothing to create.
    private var pendingModelPin: (entry: String?, model: String?, effort: String?, epoch: Int)?
    /// The last gateway-acknowledged selection (seed fetch or successful PUT)
    /// — what a failed PUT reverts the pill to. Reverting to the previous
    /// DISPLAY value would resurrect a selection the gateway never accepted
    /// (two failures in a row rest on the first failed pick).
    private var confirmedModelPin: (entry: String?, model: String?, effort: String?) = (
        nil, nil, nil
    )
    /// A draft-time pin PUT failed inside `ensureRemoteSession`; the notice is
    /// raised by the send path AFTER the send settles, because the dial path
    /// clears `notice` on its way out (the redial-supersedes-notice contract)
    /// and would wipe one raised here.
    private var draftPinFailed = false
    /// Tail of the serialized pin-PUT chain + how many are queued/in flight.
    /// Every pin write awaits the one before it: two overlapping PUTs have no
    /// wire order (pooled direct connections; the relay replays a silently
    /// failed leg after its first-byte timeout), so the older could land last
    /// and pin the gateway to a pick the pill no longer shows.
    private var modelPutTail: Task<Void, Never>?
    private var modelPutsInFlight = 0

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
    /// Recovers a DRAFT whose first send failed offline. A draft has no live leg
    /// and no reconnect loop (`connect`/`scheduleReconnect` are gated off until a
    /// real send commits it), and `ensureRemoteSession` — the network step
    /// `dispatchSend` fails on offline — is exactly what would lift it; the sweep
    /// only runs on a live leg and `resumeStrandedSends` won't fire until the app
    /// is foregrounded. So nothing else re-drives such a send in-session. This
    /// loop retries the draft recovery on a backoff until the draft resolves (the
    /// reconnect machinery + sweep take over) or the outbox drains.
    private var sendRecoveryTask: Task<Void, Never>?
    private weak var bridge: TranscriptBridge?
    private var bufferedFrames: [String] = []
    /// Set when the offscreen buffer overflowed and was dropped: the next
    /// `attachBridge` asks the webview to run its sync loop (from its own
    /// durable cursor) rather than flushing a hole-punched stream. Frames
    /// arriving meanwhile are dropped, not re-buffered.
    private var needsSyncOnAttach = false

    convenience init(sessionId: String, client: any BayboClientProtocol = Baybo.client) {
        self.init(
            sessionId: sessionId, client: client, index: .shared,
            outbox: OutboxStore(sessionId: sessionId))
    }

    init(
        sessionId: String,
        client: any BayboClientProtocol,
        index: SessionIndex,
        outbox: OutboxStore
    ) {
        self.sessionId = sessionId
        self.client = client
        self.index = index
        self.outbox = outbox
        let listed = index.contains(sessionId: sessionId)
        self.listed = listed
        connState = listed ? .connecting : .draft
        remoteSessionEnsured = false
        modelPinResolved = !listed
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
                try await client.chatConnect(
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

    /// Arm/clear the sustained-disconnect debounce on every `connState` edge.
    /// Same-value writes re-enter here (the dial loop re-asserts
    /// `.connecting`), so the armed timer is kept, not restarted — restarting
    /// would let a fast retry loop starve the signal forever.
    private func watchLegDown() {
        switch connState {
        case .connected, .draft:
            legDownTask?.cancel()
            legDownTask = nil
            if legDown { legDown = false }
        case .connecting, .offline:
            guard legDownTask == nil, !legDown else { return }
            legDownTask = Task { [weak self] in
                guard let delay = self?.legDownDelay else { return }
                try? await Task.sleep(for: delay)
                guard !Task.isCancelled, let self else { return }
                self.legDownTask = nil
                if self.connState != .connected, self.connState != .draft {
                    self.legDown = true
                }
            }
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
        sendRecoveryTask?.cancel()
        sendRecoveryTask = nil
        connectTask?.cancel()
        connectTask = nil
        // Mute every sink, including one handed to a dial still in flight.
        issuedGeneration += 1
        generation = issuedGeneration
        connState = remoteSessionEnsured ? .connecting : .draft
        // The `.connecting` write above just armed the debounce on a store
        // being torn down — disarm it (nothing will ever dial this store again).
        legDownTask?.cancel()
        legDownTask = nil
        await client.chatDisconnect()
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
        sendRecoveryTask?.cancel()
        sendRecoveryTask = nil
        connectTask?.cancel()
        connectTask = nil
        legDownTask?.cancel()
        legDownTask = nil
        issuedGeneration += 1
        generation = issuedGeneration
        await client.chatUnsubscribe(sessionId: sessionId)
    }

    /// The user left this conversation: its `.session` route dropped off
    /// `AppStore.chatPath`. Whatever was staged in the composer can never ride a
    /// send from here again, so its work is cancelled and its spools reclaimed
    /// (up to 100 MiB a pick, ten to a strip). A cover or a sheet OVER the chat
    /// is emphatically NOT this — see `staging`. The visit closes FIRST, so
    /// everything the teardown below stirs up can only retract a line, never
    /// raise one.
    func leaveChat() {
        chatOpen = false
        staging.clear()
        retractNotice()
    }

    /// The dock's line dies with the VISIT, whoever raised it. The strip retracts
    /// its own (`ComposerStaging.clear` above), but a model-pin, draft-pin, or
    /// approval failure names no tile and so has nothing to take it back. This
    /// only covers what is on the dock at the moment of leaving; everything that
    /// lands after it is refused by the `chatOpen` gate on `notice` itself. The
    /// guard is the one `ComposerView.send` documents — an unconditional write
    /// publishes an `objectWillChange` even when already nil.
    private func retractNotice() {
        guard notice != nil else { return }
        notice = nil
    }

    // MARK: - Bridge lifecycle

    func attachBridge(_ bridge: TranscriptBridge) {
        // The transcript is aimed here, so this conversation is the open one
        // again — a resident store left on a previous visit may raise notices.
        chatOpen = true
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
        // Passive observers on the same frame stream, parsed once: a user Echo
        // (ordinal-less, matching platform_msg_id) advances the outbox; the
        // turn's terminal assistant `message` updates the chat-list preview in
        // place; approval frames drive the composer's card. They run even while
        // offscreen (bridge nil, buffered) so a reply that lands after the user
        // backs out still updates the list live.
        //
        // They also run AHEAD of the overflow bail below — deliberately. The
        // buffer is the WEBVIEW's replay queue, and dropping it only costs the
        // webview a re-sync; the observers own native state the sync can't
        // rebuild. An approval in particular: `sync_page` carries rows, not
        // pending prompts, and a re-attach doesn't re-Subscribe, so a card
        // dropped here would never come back — the gate would just deny itself
        // five minutes later with the user never asked.
        if let data = frameJson.data(using: .utf8),
            let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        {
            outboxObserveFrame(frame)
            noteChatListPreview(frame)
            approvalObserveFrame(frame)
        }
        // Already overflowed while offscreen: the webview re-syncs on the next
        // attach, so don't buffer past the hole the dropped frames left.
        if bridge == nil && needsSyncOnAttach { return }
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
        func pushDemoUserSent(
            msgId: String, text: String, attachments: [AttachmentRef] = []
        ) {
            bridge?.userSent(msgId: msgId, text: text, attachments: attachments)
        }

        func pushDemoFrame(_ frameJson: String) {
            pushFrame(frameJson)
        }

        /// Through the SAME buffered emitters the real download path uses, not
        /// straight at `bridge?`. A demo drive runs on a timer from launch, but
        /// the transcript webview boots on its own schedule — several times
        /// slower on a hosted CI runner than on a dev Mac. An optional-chained
        /// push at a bridge that has not attached yet is silently DROPPED, so
        /// the cards never left `idle` and the UI smokes read it as the product
        /// failing to download. Production already solved this (the detach
        /// window); the demo just wasn't using it.
        func pushDemoFileState(
            blobId: String, state: String, loaded: UInt64? = nil, total: UInt64? = nil
        ) {
            pushFileState(blobId: blobId, state: state, loaded: loaded, total: total)
        }

        /// The web side asks for a blob and native answers, so this reply cannot
        /// outrun the bridge that carried the request — no buffer needed.
        func pushDemoBlobResult(id: Int, dataBase64: String, mimeType: String) {
            bridge?.blobResult(id: id, dataBase64: dataBase64, mimeType: mimeType, error: nil)
        }

        func pushDemoVideoPoster(
            id: Int, dataBase64: String, width: Int, height: Int, durationMs: Int
        ) {
            pushVideoPoster(
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

    /// Enqueue on the live leg (dialing first if needed). A transport failure
    /// here — no network, dial refused — is NOT (yet) a send failure: the
    /// optimistic bubble stays in its `sending` (spinner) state and the persisted
    /// outbox keeps the message queued, auto-retrying on reconnect (10 s no-echo →
    /// one blind resend, capped at 3 transmissions) and resending-after-sync on
    /// the reconnect edge. The `failed` red-dot affordance is raised only by
    /// `failOutboxEntry` — the automatic budget spent on a live leg, OR the
    /// message left queued past `queuedTimeout` on a sustained down leg — never
    /// prematurely on the single failed dial the outbox is about to retry (the old
    /// catch flagged `sendFailed` on every offline send, then the outbox auto-sent
    /// it anyway).
    private func dispatchSend(msgId: String, text: String, attachments: [AttachmentRef]) {
        Task {
            do {
                try await ensureRemoteSession()
                index.recordUserSend(
                    sessionId: sessionId, text: text, attachments: attachments)
                try await sendWhenReady(text: text, msgId: msgId, attachments: attachments)
            } catch {
                // Queued, not failed: the outbox owns redelivery (see above). A
                // draft is the one state the outbox can't drain on its own — arm
                // the in-session recovery so it delivers when the network returns
                // rather than sitting as a spinner until the next foreground.
                NSLog("baybo: send deferred to outbox: %@", bayboErrorText(error))
                scheduleSendRecovery()
            }
            surfaceDraftPinFailure()
        }
    }

    /// Raise the model notice a failed draft-time pin deferred — AFTER the
    /// send settled, because the dial path clears `notice` as it starts and
    /// would wipe one raised mid-flight (see `applyPendingModelPin`).
    private func surfaceDraftPinFailure() {
        guard draftPinFailed else { return }
        draftPinFailed = false
        notice = Lang.shared.t("chat.modelFailed")
    }

    private func sendWhenReady(
        text: String,
        msgId: String,
        attachments: [AttachmentRef]
    ) async throws {
        if connState == .connected {
            try await client.chatSend(
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
            try await client.chatSendAfterConnect(
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
            reconcileOutboxOnConnect(justSent: msgId)
        } catch {
            guard gen >= generation else { throw error }
            connState = .offline
            scheduleRetry()
            throw error
        }
    }

    /// Adopt sends a previous process enrolled. `send` writes the outbox BEFORE
    /// it touches the network — so a kill in that window leaves entries for a
    /// session the gateway was never told about and `SessionIndex` never listed,
    /// and nothing would ever drain them: `connect()` is a no-op on a draft, the
    /// sweep only fires on a connected leg, and compose mints a FRESH id, so the
    /// conversation is unreachable from the list. Ensure the row, list it, then
    /// dial: the dial's reconciliation gate looks every entry up before any
    /// resend, so a send that did land (and whose echo we simply never saw) is
    /// released rather than re-run. Best-effort — a failure retries on the next
    /// launch or foreground.
    func resumePersistedSends() {
        guard outbox.entries().contains(where: { $0.state != .failed }) else { return }
        Task {
            do {
                try await ensureRemoteSession()
                // The row exists now, so this is no longer a draft — without the
                // flip `connect()` would refuse it and the entries would strand
                // exactly as they just did.
                if connState == .draft {
                    connState = .connecting
                }
                // The kill landed before `dispatchSend`'s `recordUserSend`: the
                // list never learned about this conversation. Register it so the
                // pending message is visible while it redelivers. An already
                // listed row is left alone — `recordUserSend` would rewind its
                // preview to the user's question.
                if !index.contains(sessionId: sessionId), let text = strandedPreviewText() {
                    index.recordUserSend(sessionId: sessionId, text: text)
                }
                connect()
            } catch {
                NSLog("baybo: resume persisted sends: %@", bayboErrorText(error))
            }
            // The recovery path is what actually creates the session for an
            // offline draft send, so a draft-time pin that failed inside its
            // `ensureRemoteSession` must surface here too — not wait for the
            // user's next (possibly never) manual send.
            surfaceDraftPinFailure()
        }
    }

    /// The newest un-failed entry's text — the list preview for a conversation
    /// the registry never learned about. Attachment-only sends carry none.
    private func strandedPreviewText() -> String? {
        let text =
            outbox.entries()
            .filter { $0.state != .failed }
            .max(by: { $0.createdAt < $1.createdAt })?
            .text
        return (text?.isEmpty ?? true) ? nil : text
    }

    private func ensureRemoteSession() async throws {
        if remoteSessionEnsured { return }
        if let task = ensureRemoteSessionTask {
            try await task.value
            return
        }

        let sessionId = sessionId
        let task = Task {
            _ = try await client.chatCreateSession(sessionId: sessionId)
            // Both INSIDE the coalesced task, so every awaiter — the first
            // send, the recovery loop, a foreground resume — proceeds only
            // after the draft-time pin (if any) landed, and the first turn
            // reads it. Ensured flips BEFORE the pin PUT on purpose: a pick
            // made during that PUT must take `selectModel`'s direct branch
            // (onto the same serialized chain), not stash a pending pin that
            // nothing would ever apply again.
            remoteSessionEnsured = true
            await applyPendingModelPin()
        }
        ensureRemoteSessionTask = task
        do {
            try await task.value
            ensureRemoteSessionTask = nil
        } catch {
            ensureRemoteSessionTask = nil
            throw error
        }
    }

    // MARK: - Model pin

    /// Re-read `last_llm` from the session meta — on EVERY chat open, not once
    /// per store: this store stays cached resident, the gateway broadcasts no
    /// frame when another client re-pins the session, so the open-edge read is
    /// the only sync point. A draft has no remote row to read; its pin starts
    /// nil (authoritative) and any choice rides `pendingModelPin` instead.
    func refreshModelPin() {
        guard listed || remoteSessionEnsured, !modelPinFetchInFlight else { return }
        modelPinFetchInFlight = true
        let epoch = modelPinEpoch
        Task {
            defer { modelPinFetchInFlight = false }
            do {
                let pin = try await client.chatSessionModel(sessionId: sessionId)
                // Apply only when nothing moved locally since the read left:
                // a selection bumps the epoch, and a queued/in-flight PUT
                // means the value just read is already stale — applying it
                // would clobber the optimistic pill with the pre-PUT state.
                if modelPinEpoch == epoch, modelPutsInFlight == 0 {
                    modelPin = pin.llm
                    modelPinModel = pin.model
                    modelPinEffort = pin.effort
                    confirmedModelPin = (pin.llm, pin.model, pin.effort)
                    modelPinResolved = true
                }
            } catch {
                NSLog("baybo: session model: %@", bayboErrorText(error))
            }
        }
    }

    /// Pin the session to LLM entry `entry` + model `model` within it,
    /// KEEPING the current reasoning effort. (`entry == nil` ⇒ follow
    /// `default-llm`; `model == nil` ⇒ the entry's default model.)
    func selectModel(entry: String?, model: String?) {
        applySelection(entry: entry, model: model, effort: modelPinEffort)
    }

    /// Pin the session to (entry, model) AND set its reasoning effort — the
    /// chat header's thinking level, PER-SESSION (no global entry edit). The
    /// panel calls this with the entry+model the Thinking submenu sits under.
    func selectEffort(entryName: String, model: String, effort: String, catalog _: ModelCatalog) {
        applySelection(entry: entryName, model: model, effort: effort)
    }

    /// The one selection path. Optimistic: the pill flips immediately; a
    /// failed PUT reverts to the last gateway-acknowledged triple and raises
    /// the composer notice. Applies from the session's next turn (gateway
    /// contract). A no-op when the resolved pin already equals the pick.
    private func applySelection(entry: String?, model: String?, effort: String?) {
        guard (entry, model, effort) != (modelPin, modelPinModel, modelPinEffort)
            || !modelPinResolved
        else { return }
        modelPinEpoch += 1
        let epoch = modelPinEpoch
        modelPin = entry
        modelPinModel = model
        modelPinEffort = effort
        modelPinResolved = true
        guard listed || remoteSessionEnsured else {
            pendingModelPin = (entry: entry, model: model, effort: effort, epoch: epoch)
            return
        }
        enqueueModelPut { [weak self] in
            await self?.putModelPin(
                entry: entry, model: model, effort: effort, epoch: epoch, deferNotice: false)
        }
    }

    /// Deliver a draft-time selection now that the session exists — runs
    /// inside `ensureRemoteSession`'s coalesced task, BEFORE any awaiting send
    /// proceeds, so the first turn's actor already reads the pin. A pick that
    /// was superseded while the draft sat unsent is skipped: the newer
    /// selection's own PUT covers the wire. A pure-effort pick (no entry) is
    /// still applied — effort is independent of the llm/model pin. Failure
    /// degrades to the default: revert the pill and flag `draftPinFailed` for
    /// the send path to surface, never block the send. (The notice can't be
    /// raised here: the dial that follows clears `notice` as it starts.)
    private func applyPendingModelPin() async {
        guard let pending = pendingModelPin else { return }
        pendingModelPin = nil
        guard pending.epoch == modelPinEpoch else { return }
        guard pending.entry != nil || pending.effort != nil else { return }
        await enqueueModelPut { [weak self] in
            await self?.putModelPin(
                entry: pending.entry, model: pending.model, effort: pending.effort,
                epoch: pending.epoch, deferNotice: true)
        }.value
    }

    /// One serialized pin write. On failure — if no newer selection has moved
    /// the epoch — the pill reverts to the last gateway-acknowledged triple.
    private func putModelPin(
        entry: String?, model: String?, effort: String?, epoch: Int, deferNotice: Bool
    ) async {
        do {
            try await client.chatSetSessionModel(
                sessionId: sessionId, llm: entry, model: model, effort: effort)
            if modelPinEpoch == epoch { confirmedModelPin = (entry, model, effort) }
        } catch {
            NSLog("baybo: set session model: %@", bayboErrorText(error))
            guard modelPinEpoch == epoch else { return }
            modelPin = confirmedModelPin.entry
            modelPinModel = confirmedModelPin.model
            modelPinEffort = confirmedModelPin.effort
            if deferNotice {
                draftPinFailed = true
            } else {
                notice = Lang.shared.t("chat.modelFailed")
            }
        }
    }

    /// Append a pin write to the chain: it runs only after every earlier one
    /// finished, so writes reach the gateway in pick order.
    @discardableResult
    private func enqueueModelPut(_ op: @escaping () async -> Void) -> Task<Void, Never> {
        modelPutsInFlight += 1
        let previous = modelPutTail
        let task = Task {
            await previous?.value
            await op()
            modelPutsInFlight -= 1
        }
        modelPutTail = task
        return task
    }

    // MARK: - Bridge callbacks (webview → native)

    /// The one forward-recovery pull. The webview posts its cursor (`nil` =
    /// baseline); native fetches `GET …/sync?since_ordinal=…&limit=…` over the
    /// active leg, reconciles the outbox against the returned rows, then pushes
    /// the `sync_page` frame back for the webview to apply.
    func requestSync(sinceOrdinal: Int64?, limit: UInt32) {
        guard listed || remoteSessionEnsured else {
            #if DEBUG
                // A demo fixture pushes a canned turn into this very draft, and
                // the empty baseline page below would REPLACE it away (see
                // `isDrivingCannedTurn`). Unwind the webview's in-flight guard
                // without touching the thread — `sync_failed` does exactly that.
                if Self.isDrivingCannedTurn {
                    pushSynthesizedFrame([
                        "kind": "sync_failed",
                        "error": "demo fixture: no gateway to sync against",
                    ])
                    return
                }
            #endif
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
                let frame = try await client.chatFetchSync(
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
                try await client.chatMarkRead(sessionId: sessionId, ordinal: ordinal)
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
                try await client.chatSend(
                    sessionId: sessionId, text: Self.stopCommand, msgId: msgId, attachments: [])
            } catch {
                NSLog("baybo: stop: %@", bayboErrorText(error))
            }
        }
    }

    func fetchHistory(beforeOrdinal: Int64?, limit: UInt32) {
        Task {
            do {
                let frame = try await client.chatFetchHistory(
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

    /// Drive the pending-approval queue off the frame stream — the composer's
    /// approval card. Native, not web: it runs BEFORE the frame reaches (or
    /// misses) the webview, so a prompt raised while the user is parked on the
    /// chat list still lands, and an offscreen buffer overflow can't drop the
    /// only way to answer a gate that denies itself after 5 minutes. The four
    /// inputs and their id semantics live in `ApprovalQueue`.
    private func approvalObserveFrame(_ frame: [String: Any]) {
        guard approvals.apply(frame) else { return }
        pendingApprovals = approvals.cards
    }

    /// The user answered. Dismiss optimistically (the server's `approval_resolved`
    /// fan-out confirms it, and re-adding the card on the frame's return would
    /// flash a prompt they just answered), then echo the decision on the live
    /// leg. A leg that can't carry it surfaces a notice: the decision is lost and
    /// the gate will deny on its own timeout, so the user has to know.
    func resolveApproval(_ approval: PendingApproval, decision: ApprovalDecision) {
        dropApproval(callId: approval.callId)
        Haptics.tap()
        #if DEBUG
            if resolveDemoApprovalIfRequested(approval, decision: decision) { return }
        #endif
        Task {
            do {
                try await client.chatResolveApproval(
                    callId: approval.callId, decision: decision)
            } catch {
                notice = Lang.shared.t("chat.approvalFailed", bayboErrorText(error))
            }
        }
    }

    private func dropApproval(callId: String) {
        guard approvals.resolve(callId: callId) else { return }
        pendingApprovals = approvals.cards
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
        index.recordAgentReply(sessionId: sessionId, text: content)
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
                let lookup = try await client.chatLookupMessage(
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
    /// `justSent` is the message this very dial carried (`sendWhenReady`), if
    /// any — it is exempt, see below.
    private func reconcileOutboxOnConnect(justSent: String? = nil) {
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
            // The send that DROVE this dial is microseconds old: the gateway has
            // not persisted it yet, so the lookup races its own write, answers
            // `found: false`, and `resumeSending` re-arms it for a blind resend
            // at the next 3 s tick — spending a transmission on a message still
            // in flight, well inside the 10 s the echo needs. The sweep already
            // owns it; every OTHER entry rode a previous leg and is fair game.
            guard entry.platformMsgId != justSent else { continue }
            resolveUnknownEntry(platformMsgId: entry.platformMsgId)
        }
        startOutboxTimerIfNeeded()
    }

    /// One (re)transmission of an outbox entry on the live leg (same
    /// `platform_msg_id`, safe inside the gateway's dedup window). Replays the
    /// entry's attachments too, so a retried image/file send stays that message
    /// rather than degrading to text-only. Only `sweepOutbox` calls this, and it
    /// holds the `connected` check — a second guard here would silently swallow
    /// the transmission it was already counted for.
    private func resendOutboxEntry(_ entry: OutboxEntry) {
        outbox.recordTransmission(platformMsgId: entry.platformMsgId)
        let attachments = entry.attachments.map(Self.toAttachmentRef)
        Task {
            do {
                try await client.chatSend(
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

    /// A committed send failed to reach the wire on a DRAFT store — the one case
    /// nothing else re-drives in-session (see `sendRecoveryTask`). Retry the
    /// draft recovery (`resumePersistedSends`: ensure the session, dial,
    /// reconcile) on a backoff until the draft resolves — at which point the
    /// reconnect loop and sweep own redelivery — or the outbox drains. A single
    /// loop regardless of how many draft sends fail; a no-op for a non-draft
    /// store, which already has a live/reconnecting leg to drain the outbox.
    private func scheduleSendRecovery() {
        guard connState == .draft, sendRecoveryTask == nil else { return }
        sendRecoveryTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.reconnectBackoff)
                guard let self else { return }
                if self.stepSendRecovery() { return }
            }
        }
    }

    /// One recovery attempt (`resumePersistedSends`: ensure the session, dial,
    /// reconcile). Returns `true` when the loop should stop — the draft resolved
    /// (the reconnect machinery + sweep own redelivery from here) or the outbox
    /// drained. Not `private`: the recovery loop is its only production caller,
    /// but a test steps it directly rather than sleeping through
    /// `reconnectBackoff`.
    @discardableResult
    func stepSendRecovery() -> Bool {
        guard connState == .draft,
            outbox.entries().contains(where: { $0.state != .failed })
        else {
            sendRecoveryTask = nil
            return true
        }
        resumePersistedSends()
        return false
    }

    /// The retry sweep: while there are `sending`/`sent` entries, every ~3 s
    /// resend one whose 10 s echo window lapsed or fail one that exhausted the
    /// 3-transmission cap (live leg); on a down leg, fail one that outlived the
    /// `queuedTimeout`. Self-stops when the outbox drains. The `Task` inherits
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

    /// One retry-sweep pass. Returns `true` when the timer should stop — the
    /// outbox is idle. Not `private`: `outboxTimer` is its only production
    /// caller, but a test steps it directly rather than sleeping through
    /// `retryTick` on every assertion.
    @discardableResult
    func sweepOutbox() -> Bool {
        var active = false
        for entry in outbox.entries() {
            guard entry.state == .sending || entry.state == .sent else { continue }
            active = true
            if entry.state != .sending { continue }
            if connState == .connected {
                // A transmission is only ever spent on the wire: resend one whose
                // echo window lapsed, or fail one that burned its budget.
                if outbox.dueForBlindResend(entry) {
                    resendOutboxEntry(entry)
                } else if outbox.resendExhausted(entry) {
                    failOutboxEntry(entry.platformMsgId)
                }
            } else if outbox.queuedTooLong(entry) {
                // Down leg: nothing to resend on, but a message queued past its
                // lifetime cap IS a send failure — surface the red dot the user
                // retries by hand instead of a spinner that never resolves. The
                // sweep keeps ticking until then (not parked), so the timeout
                // actually fires; a brief outage still delivers on reconnect,
                // well inside the cap.
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
                let bytes = try await client.blobDownloadBytes(
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
            let cached = await client.blobIsCached(blobId: blobId)
            pushFileState(blobId: blobId, state: cached ? "ready" : "idle")
        }
    }

    func downloadFile(blobId: String) {
        guard fileDownloads.insert(blobId).inserted else { return }
        pushFileState(blobId: blobId, state: "loading", loaded: 0)
        Task {
            defer { fileDownloads.remove(blobId) }
            do {
                _ = try await client.blobDownloadBytes(
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
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                // Backed out mid-materialise: don't arm a stale sheet for the
                // next entry (same on-screen token as playVideo/shareFile).
                guard bridge != nil else { return }
                filePreview = FilePreview(url: url)
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// A card long-press: hand the blob to the system share sheet under its
    /// real name, so Files / AirDrop / Save-to-Photos keep the original bytes
    /// and name — the same materialisation the previewer and players use.
    /// (Images share from inside their viewer instead.)
    func shareFile(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                // Backed out mid-materialise: don't arm a stale sheet for the
                // next entry (same on-screen token as playVideo).
                guard bridge != nil else { return }
                fileShare = FilePreview(url: url)
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
            guard let bytes = try? await client.blobDownloadBytes(
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
        #if DEBUG
            // Demo blobs have no gateway to fetch from; a locally-served stand-in
            // lets share/preview present headlessly (see DemoFrames).
            if let demo = Self.demoMaterializeBytes(blobId: blobId) {
                try demo.write(to: url, options: .atomic)
                return url
            }
        #endif
        if let inFlight = previewMaterializations[url] {
            return try await inFlight.value
        }
        let task = Task {
            let bytes = try await client.blobDownloadBytes(blobId: blobId, progress: nil)
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
