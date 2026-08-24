import Foundation
import UIKit
import WebKit

/// The Swift half of the transcript bridge (the JS half lives in
/// `web/src/bridge.ts` — the contract is documented there and in
/// docs; keep the two in sync).
///
/// Native→web calls are `window.baybo.*` evaluations, buffered until the web
/// side posts `ready` (the bundle installs `window.baybo` synchronously, but
/// eval before the page commits would vanish). Web→native messages arrive on
/// the `baybo` script message handler as `{ type: ... }` objects.
@MainActor
final class TranscriptBridge: NSObject, ObservableObject, WebMediaSink {
    static let messageHandlerName = "baybo"

    private weak var store: (any TranscriptTarget)?
    weak var webView: WKWebView?
    private(set) var ready = false
    private var pending: [String] = []
    /// Which conversation the mounted page is rendering. Held here rather than
    /// read off `store` because `store` is weak: the LRU can evict and
    /// deallocate a session's store while its transcript is still the page in
    /// the webview, and `<Transcript>` is keyed on the session id — so a
    /// re-entry that re-inits the SAME id does not remount the React tree, and
    /// a resync that skipped the reload here would leave the stale rows up.
    private(set) var shownSessionId: String?
    /// Refuse mirror writes until the rebuilt page is up. The outgoing
    /// document's `pagehide` flushes its debounced `persist`, which would write
    /// back the very state the resync just deleted — and `deliverInit` would
    /// then restore it.
    private var discardPersist = false
    private var lastBottomInset = Int.min
    private var composerTop: CGFloat?
    /// Mirror of the web transcript's `showJump` state — drives the native
    /// glass jump-to-latest button above the composer.
    @Published private(set) var jumpVisible = false
    /// The header's message index, mirrored from the transcript's `outline`
    /// post — the user's own sends, each with the agent's reply as a gloss. The
    /// web side owns the derivation (it alone sees this device's optimistic and
    /// offline bubbles); this side only renders and jumps.
    @Published private(set) var outline: [OutlineEntry] = []
    @Published private(set) var outlineHasMoreOlder = false
    @Published private(set) var outlineLoadingOlder = false
    /// The outline payload failed to decode — a contract break on the far side
    /// of a seam whose marshalling fails silently. The header button is
    /// PERMANENT and stays put; this only changes what the empty sheet says,
    /// so a thread full of messages is never told "you have never sent one".
    /// (It replaces an older gate that hid the button below three entries; the
    /// button's position is worth more than the gate ever was.)
    @Published private(set) var outlineFailed = false
    /// The entry the transcript is currently parked on, so the sheet can mark
    /// where the reader already is.
    @Published private(set) var outlineHereId: String?
    /// This conversation has delegated to at least one subagent, derived from
    /// the rendered rows rather than fetched — the web side sees a
    /// `spawn_subagent` work step and says so, which costs no network and is
    /// correct offline. Latches within a session (a page of history with no
    /// spawn must not retract the header entry); `resetOutline` clears it when
    /// the webview changes conversation.
    @Published private(set) var subagentsPresent = false
    /// `false` until the transcript has painted its first frame (`shown`), so
    /// the webview can fade in rather than pop its content in as the chat
    /// screen slides on. Re-armed on every fresh page load (`ready`).
    @Published private(set) var contentVisible = false
    /// An agent-authored HTML iframe is expanded over the transcript. Native
    /// hides its header/composer while true; the close control lives in the
    /// trusted parent document, never inside the untrusted iframe.
    @Published private(set) var htmlPreviewMaximized = false

    init(store: any TranscriptTarget) {
        self.store = store
        shownSessionId = store.sessionId
        super.init()
        store.attachBridge(self)
    }

    func retarget(to newStore: any TranscriptTarget) {
        if store === newStore {
            newStore.attachBridge(self)
        } else {
            collapseHtmlPreview()
            // A DIFFERENT store for the same conversation (the LRU evicted this
            // session's store and re-opening minted a fresh one) keeps the very
            // same React tree — `<Transcript>` is keyed on `sessionId`, so `init`
            // re-renders nothing and the outline effect never re-fires. Clearing
            // the index there would blank it with nothing left to re-post it.
            let sameSession = store?.sessionId == newStore.sessionId
            if let old = store {
                call("flushPersist", "")
                old.detachBridge(self)
            }
            store = newStore
            // Hiding here can pause WebKit rAF and strand the `shown` callback.
            contentVisible = true
            deliverInit()
            newStore.attachBridge(self)
            lastBottomInset = Int.min
            pushBottomInset()
            jumpVisible = false
            if !sameSession { resetOutline() }
        }
        // Last, so the re-seeded bubbles land at the tail — where an optimistic
        // send always sits — behind whatever the attach just flushed. Skipped
        // while the page is still loading: those calls would only queue in
        // `pending`, and the `ready` that flushes them re-seeds again — the same
        // work twice, arriving before the web side has committed the first pass
        // and can recognise it.
        if ready { newStore.replayUnconfirmedSends(to: self) }
    }

    /// The other half of `subagentsPresent`. The web side only sees the rows it
    /// has LOADED, and the spawning turn leaves that window routinely (a
    /// REPLACE rebuilds from the newest page), so entering a conversation also
    /// asks the gateway once. Latching here rather than assigning keeps the two
    /// sources OR-ed.
    func noteSubagentsPresent() {
        subagentsPresent = true
    }

    /// ONE webview serves every conversation, so an index left behind renders
    /// over the next session's header — and its rows jump into a thread that no
    /// longer holds them. Safe to call on a page reload too: that remounts the
    /// React tree, whose mount effect posts a fresh outline.
    private func resetOutline() {
        outline = []
        outlineHasMoreOlder = false
        outlineLoadingOlder = false
        outlineFailed = false
        outlineHereId = nil
        subagentsPresent = false
    }

    /// Rebuild the page from scratch — the second half of `ChatStore.resync`,
    /// taken only when the mounted page is that session's. Any other session's
    /// resync needs no reload: its next open re-inits under a different key and
    /// remounts the React tree with the mirror already gone.
    ///
    /// The same load `TranscriptHost.init` performs, so every piece of
    /// in-memory web state dies with the document: the rows, the sync cursor,
    /// and above all the live latches (an open work block, `turnActive`, the
    /// streaming buffer). A "reset yourself" bridge message is deliberately NOT
    /// what this is — it could only clear the state we thought to enumerate,
    /// and state that was not cleared when it should have been is exactly what
    /// the hatch exists to escape.
    ///
    /// The leg is untouched: this session stays subscribed, so frames keep
    /// arriving through the reload (they buffer in `pending` and flush after
    /// the fresh `init`).
    func rebuildIfShowing(_ sessionId: String) {
        guard shownSessionId == sessionId else { return }
        // The mirror was just deleted; a late persist from the dying document
        // would resurrect it.
        discardPersist = true
        reloadDocument()
    }

    /// How many back-to-back deaths `contentProcessDied` will answer with a
    /// reload before giving up. The kill is memory pressure, and the reload
    /// rebuilds the same footprint — with no cap, a page that dies on every
    /// load flickers forever while hammering the gateway with mount-edge
    /// syncs. Past the cap the transcript stays blank until the user backs
    /// out or resyncs; a successful first paint (`shown`) re-arms the budget.
    private static let maxConsecutiveDeaths = 3
    private static let deathWindowSeconds: TimeInterval = 30
    private var consecutiveDeaths = 0
    private var lastDeathAt = Date.distantPast

    /// The WebContent process died under a VISIBLE webview — the one case
    /// WebKit does NOT auto-reload (an offscreen kill heals itself on
    /// re-attach: the reload's fresh `ready` re-inits and re-seeds). Without
    /// this, `ready` stays latched true and every call() silently no-ops
    /// against a blank page — the transcript is bricked until a resync or an
    /// app restart. Same recovery as `rebuildIfShowing` minus the mirror drop:
    /// the mirror is intact and IS what the fresh `ready`'s init restores
    /// (which is also why `discardPersist` stays false here — a late persist
    /// from the dead process carries the freshest pre-crash state, exactly
    /// what the mirror should hold).
    func contentProcessDied() {
        let now = Date()
        if now.timeIntervalSince(lastDeathAt) > Self.deathWindowSeconds {
            consecutiveDeaths = 0
        }
        lastDeathAt = now
        consecutiveDeaths += 1
        guard consecutiveDeaths <= Self.maxConsecutiveDeaths else {
            NSLog("baybo: transcript web content process died again; giving up on reloads")
            return
        }
        NSLog("baybo: transcript web content process died; reloading")
        reloadDocument()
    }

    /// The one reload body both hatches share, so a latch added to one reset
    /// list cannot silently miss the other. What it deliberately does NOT
    /// own is `discardPersist` — whether a late persist from the dying
    /// document is welcome is the one load-bearing difference between the
    /// callers, and it must stay visible at each call site.
    private func reloadDocument() {
        guard let webView, let url = TranscriptSchemeHandler.indexURL else { return }
        ready = false
        // Buffered calls target a document about to be destroyed — and one of
        // them may be an `init` carrying state the new document must rebuild
        // for itself.
        pending.removeAll()
        htmlPreviewMaximized = false
        webView.load(URLRequest(url: url))
    }

    func detachCurrent(_ leaving: any TranscriptTarget) {
        // SwiftUI can run the next screen's `onAppear` before this `onDisappear`.
        guard store === leaving else { return }
        collapseHtmlPreview()
        call("flushPersist", "")
        store?.detachBridge(self)
    }

    // MARK: - Native → web

    func pushFrame(_ frameJson: String) {
        call("pushFrame", jsonLiteral(frameJson))
    }

    func setConnEpoch(_ epoch: Int) {
        call("setConnEpoch", String(epoch))
    }

    /// Ask the webview to run its sync loop (from its own cursor) — the
    /// offscreen-buffer-overflow re-attach edge, where native dropped the live
    /// frames and the durable record must be re-pulled.
    func requestSync() {
        call("requestSync", "")
    }

    func userSent(msgId: String, text: String, attachments: [AttachmentRef]) {
        let payload: [String: Any] = [
            "msgId": msgId,
            "text": text,
            "attachments": attachments.map { ref in
                var dict: [String: Any] = [
                    "kind": kindString(ref.kind),
                    "blob_id": ref.blobId,
                    "mime_type": ref.mimeType,
                    "size": ref.size,
                ]
                if let filename = ref.filename {
                    dict["filename"] = filename
                }
                return dict
            },
        ]
        call("userSent", jsonObjectLiteral(payload))
    }

    /// A send Task errored — flag that optimistic bubble failed (red retry dot),
    /// keyed by the msgId minted in the matching `userSent`.
    func sendFailed(_ msgId: String) {
        call("sendFailed", jsonLiteral(msgId))
    }

    /// The outbox released that send — it is durable, so the transcript may stop
    /// overlaying its optimistic bubble across a REPLACE. The durable row's
    /// ordinal rides along (when the proof carried one) so the bubble gains
    /// sync coverage the echo never gave it.
    func sendConfirmed(_ msgId: String, ordinal: Int64?) {
        call("sendConfirmed", "\(jsonLiteral(msgId)), \(ordinal.map(String.init) ?? "null")")
    }

    func blobResult(id: Int, dataBase64: String?, mimeType: String, error: String?) {
        let payload: [String: Any] = [
            "id": id,
            "dataBase64": dataBase64 as Any,
            "mimeType": mimeType,
            "error": error as Any,
        ]
        call("blobResult", jsonObjectLiteral(payload))
    }

    /// One file card's lifecycle step. `loaded`/`total` only ride a `loading`
    /// tick; `error` only a `failed` one.
    func fileState(
        blobId: String, state: String, loaded: UInt64?, total: UInt64?, error: String?
    ) {
        var payload: [String: Any] = ["blobId": blobId, "state": state]
        if let loaded { payload["loaded"] = loaded }
        if let total { payload["total"] = total }
        if let error { payload["error"] = error }
        call("fileState", jsonObjectLiteral(payload))
    }

    /// One audio track's engine state (see `AudioPlayerCenter`): play/pause
    /// flips, 2 Hz position ticks, and the `stopped` reset on end/usurp.
    func audioState(blobId: String, state: String, position: Double, duration: Double) {
        let payload: [String: Any] = [
            "blobId": blobId,
            "state": state,
            "position": position,
            "duration": duration,
        ]
        call("audioState", jsonObjectLiteral(payload))
    }

    /// Answer to `requestVideoPoster`: the poster frame's JPEG bytes plus the
    /// natural size and duration, or `dataBase64: nil` + error.
    func videoPoster(
        id: Int, dataBase64: String?, width: Int, height: Int, durationMs: Int, error: String?
    ) {
        let payload: [String: Any] = [
            "id": id,
            "dataBase64": dataBase64 as Any,
            "width": width,
            "height": height,
            "durationMs": durationMs,
            "error": error as Any,
        ]
        call("videoPoster", jsonObjectLiteral(payload))
    }

    func setLanguage(_ lang: String) {
        call("setLanguage", jsonLiteral(lang))
    }

    /// The composer's top edge in `.global` (window) coordinates. Fires once
    /// per keyboard/composer settle, at the animation START (SwiftUI geometry
    /// jumps to the target); the web side animates its padding to match the
    /// keyboard's slide (the webview's own frame never moves).
    func setComposerTop(_ minYInWindow: CGFloat) {
        composerTop = minYInWindow
        pushBottomInset()
    }

    /// Convert the composer edge to the covered-strip height against the
    /// WINDOW bottom (`.global` is window space; UIScreen would over-inset any
    /// non-fullscreen iPad window). Deduped on whole pixels; the dedup — and
    /// the value itself — is replayed from the `ready` handler because a
    /// jetsammed web process silently reloads to a page whose inset var is
    /// back at 0 while the composer geometry never re-fires.
    private func pushBottomInset() {
        guard let composerTop, let window = webView?.window else { return }
        let px = max(0, Int((window.bounds.height - composerTop).rounded()))
        guard px != lastBottomInset else { return }
        lastBottomInset = px
        call("setBottomInset", String(px))
    }

    /// A native jump-button tap runs the web side's glide (scroll state and
    /// settle logic live with the scroll container).
    func jumpToLatest() {
        call("jumpToLatest", "")
    }

    /// Park the transcript on one of the user's own messages (an index row tap).
    /// The web side owns the scroll and the ring bloom, as with `jumpToLatest`.
    func jumpToMessage(_ rowId: String) {
        call("jumpToMessage", jsonLiteral(rowId))
    }

    /// Park the transcript on a search hit, addressed BY ORDINAL.
    ///
    /// Not a row id: a user row is keyed by its `platform_msg_id` with the
    /// ordinal carried beside it, so building `m<ordinal>` here would resolve
    /// agent rows and silently miss every user-authored hit — most of what a
    /// search finds.
    ///
    /// Fire-and-forget, and deliberately stateful on the far side. `call`'s
    /// pending queue covers "the webview has not loaded yet", but the ordinary
    /// case is a webview that IS loaded and simply has not paged back far
    /// enough — so the web side holds the request and pages toward it, giving up
    /// against its own budget. Nothing here waits on the outcome.
    func jumpToOrdinal(_ ordinal: Int64) {
        call("jumpToOrdinal", String(ordinal))
    }

    /// Page one more screen of older rows into the thread, so the index can
    /// reach further back than the thread currently holds.
    func outlineLoadOlder() {
        call("outlineLoadOlder", "")
    }

    /// Ask which entry the reader is parked on right now — answered by an
    /// `outlineHere` post. The previous answer is dropped up front: the reply is
    /// a round trip through the web-content process, so a sheet that read the
    /// value as it presented would otherwise scroll to the LAST visit's
    /// position and never correct itself.
    func requestOutlineHere() {
        outlineHereId = nil
        call("requestOutlineHere", "")
    }

    private func collapseHtmlPreview() {
        htmlPreviewMaximized = false
        call("collapseHtmlPreview", "")
    }

    /// The left-edge swipe over a full-screen preview, streamed to the page that
    /// draws it. The pop is held off for the duration (`EdgeSwipeOverride`), so
    /// this is the ONLY thing the gesture can mean while one is up.
    ///
    /// Guarded on the maximized flag rather than fired blind: the flag drops the
    /// instant the web side commits to a dismissal, and a `move` that lands
    /// after that would re-address a box already on its way out.
    func htmlPreviewDragBegin() {
        guard htmlPreviewMaximized else { return }
        call("htmlPreviewDragBegin", "")
    }

    func htmlPreviewDragMove(_ points: CGFloat) {
        guard htmlPreviewMaximized else { return }
        call("htmlPreviewDragMove", String(Int(points.rounded())))
    }

    /// Native judged the release, so native retires the flag — the web echo
    /// that follows is confirmation, not the trigger.
    ///
    /// This is the screen's escape hatch, and it must not depend on the web
    /// process. While the flag is up there is no back button, no composer and
    /// no interactive pop, so every way out of the conversation runs through a
    /// page rendering ARBITRARY agent-authored script. One `while(true)` in
    /// there and a swipe whose dismissal only the page can conclude would leave
    /// nothing but a force-quit. (A jetsammed web process heals itself — the
    /// reload's `ready` clears the flag — but a wedged one never does.)
    /// Behaviour-neutral on the happy path: the page posts `false` at the very
    /// start of its exit animation anyway, i.e. this same instant.
    func htmlPreviewDragEnd(dismiss: Bool) {
        guard htmlPreviewMaximized else { return }
        call("htmlPreviewDragEnd", dismiss ? "true" : "false")
        if dismiss { htmlPreviewMaximized = false }
    }

    #if DEBUG
        /// `-baybo-demo-jump`: 4s in, shove the document off the newest edge from
        /// inside the page — the REAL window scroll → showJump → `jumpVisible`
        /// mirror fires — then 3s later run the native jump path. The glass
        /// button's full round trip, screenshot-verifiable headlessly (pair
        /// with -baybo-open-chat -baybo-demo-frames). Scrolls the MAIN FRAME
        /// (the single scroller); `.chat-log` no longer scrolls.
        func startDemoJumpIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-jump") else { return }
            Task { @MainActor in
                try? await Task.sleep(for: .seconds(4))
                evaluate(
                    "window.scrollBy({top: -1400, behavior: 'instant'});"
                )
                try? await Task.sleep(for: .seconds(3))
                jumpToLatest()
            }
        }
    #endif

    private func deliverInit() {
        guard let store else { return }
        shownSessionId = store.sessionId
        let restored: String?
        if store.mirrored {
            restored = TranscriptStore.read(sessionId: store.sessionId)
        } else {
            // A read-only page renders the server's CURRENT truth only. Delete
            // rather than merely skip: installs that viewed this child against
            // an old gateway already hold a mirror full of rows the fixed read
            // path no longer serves, and nothing else would ever remove it.
            TranscriptStore.delete(
                sessionId: store.sessionId, in: SessionIndex.supportDirectory())
            restored = nil
        }
        // The in-app language override (falls back to the device language) —
        // the same source the native chrome renders from, so the two can't
        // diverge.
        let language = Lang.shared.code
        let payload = """
            {"language":\(jsonLiteral(language)),\
            "sessionId":\(jsonLiteral(store.sessionId)),\
            "restoredState":\(restored ?? "null"),\
            "connEpoch":\(store.connEpoch)}
            """
        call("init", payload)
    }

    private func call(_ method: String, _ argumentLiteral: String) {
        let js = "window.baybo && window.baybo.\(method)(\(argumentLiteral));"
        guard ready, webView != nil else {
            pending.append(js)
            return
        }
        evaluate(js)
    }

    private func flushPending() {
        for js in pending {
            evaluate(js)
        }
        pending.removeAll()
    }

    /// A thrown exception inside an evaluated call surfaces ONLY through the
    /// completion handler (window.onerror never sees it) — log it, or bridge
    /// failures are invisible.
    private func evaluate(_ js: String) {
        webView?.evaluateJavaScript(js) { _, error in
            if let error {
                NSLog("baybo: bridge eval failed: %@", error.localizedDescription)
            }
        }
    }

    private func kindString(_ kind: AttachmentKind) -> String {
        switch kind {
        case .image: return "image"
        case .audio: return "audio"
        case .file: return "file"
        }
    }

    /// A Swift string as a JS string literal (JSON-escaped).
    private func jsonLiteral(_ value: String) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: [value]),
            let wrapped = String(data: data, encoding: .utf8)
        else {
            return "\"\""
        }
        // Strip the array brackets that made the fragment serializable.
        return String(wrapped.dropFirst().dropLast())
    }

    private func jsonObjectLiteral(_ object: [String: Any]) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: object),
            let json = String(data: data, encoding: .utf8)
        else {
            return "{}"
        }
        return json
    }
}

extension TranscriptBridge: TranscriptSurface {}

// MARK: - Web → native

extension TranscriptBridge: WKScriptMessageHandler {
    nonisolated func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        // WKScriptMessageHandler always delivers on the main thread; assume the
        // actor rather than re-dispatch so bridge messages keep their order
        // relative to each other and to frame pushes.
        MainActor.assumeIsolated {
            // WKWebView exposes a script handler in every subframe. Agent HTML
            // runs in a sandboxed iframe and must never drive native actions
            // directly; only the trusted transcript main frame owns this seam.
            guard message.frameInfo.isMainFrame,
                message.name == Self.messageHandlerName,
                let body = message.body as? [String: Any],
                let type = body["type"] as? String
            else { return }
            self.handle(type: type, body: body)
        }
    }

    private func handle(type: String, body: [String: Any]) {
        switch type {
        case "ready":
            NSLog("baybo: transcript bridge ready (session=%@)", store?.sessionId ?? "?")
            ready = true
            deliverInit()
            flushPending()
            // Fresh pages reset CSS vars, so replay inset past the dedup.
            lastBottomInset = Int.min
            pushBottomInset()
            jumpVisible = false
            resetOutline()
            contentVisible = store?.listed == false
            // Queued durability confirms first, then the outbox replay: the
            // confirm retires ids the replay must not re-seed, and neither may
            // sit in `pending` where a rebuild/crash reload would destroy the
            // transcript half of a release whose outbox half already committed.
            store?.flushPendingSendConfirms(to: self)
            // After the flush, so the live frames that arrived during the load
            // keep their arrival order and the re-seeded bubbles land at the
            // tail — where an optimistic send always sits.
            store?.replayUnconfirmedSends(to: self)
            discardPersist = false
            htmlPreviewMaximized = false
        case "shown":
            // The transcript painted its first frame — fade the webview in.
            // A real paint also proves the reloaded process is standing, so
            // the crash-reload budget re-arms.
            consecutiveDeaths = 0
            contentVisible = true
        case "sync":
            // The one forward-recovery pull: the webview posts its cursor
            // (`sinceOrdinal` null = baseline) and the page size it elected.
            let since = (body["sinceOrdinal"] as? NSNumber)?.int64Value
            let limit = (body["limit"] as? NSNumber)?.uint32Value ?? 50
            store?.requestSync(sinceOrdinal: since, limit: limit)
        case "mark_read":
            // The viewer has read up to `ordinal` — advance the server chat-list
            // read cursor so the unread badge clears on the next list pull.
            if let ordinal = (body["ordinal"] as? NSNumber)?.int64Value {
                store?.markRead(ordinal: ordinal)
            }
        case "persist":
            // A resync rebuild is in flight: this is the outgoing document's
            // `pagehide` flush, and honouring it would restore the mirror the
            // hatch just deleted, with exactly the state being thrown away.
            if discardPersist { break }
            // Persist is async and may arrive after the bridge retargets.
            // A bridge whose target opted out of mirroring hosts only such
            // targets (the subagent sheet's own webview), so drop the write for
            // ANY session id — a late flush from the previously shown child
            // must not mint a mirror either. deliverInit's delete is the
            // backstop for a flush that slips through a mid-retarget nil store.
            if let store, !store.mirrored { break }
            if let sessionId = (body["sessionId"] as? String) ?? store?.sessionId,
                let state = body["state"],
                let data = try? JSONSerialization.data(withJSONObject: state),
                let json = String(data: data, encoding: .utf8)
            {
                TranscriptStore.write(sessionId: sessionId, stateJson: json)
            }
        case "fetchHistory":
            let before = (body["beforeOrdinal"] as? NSNumber)?.int64Value
            let limit = (body["limit"] as? NSNumber)?.uint32Value ?? 50
            store?.fetchHistory(beforeOrdinal: before, limit: limit)
        case "requestBlob":
            if let id = (body["id"] as? NSNumber)?.intValue,
                let blobId = body["blobId"] as? String
            {
                store?.requestBlob(id: id, blobId: blobId)
            }
        case "queryFileState":
            if let blobId = body["blobId"] as? String {
                store?.queryFileState(blobId: blobId)
            }
        case "downloadFile":
            if let blobId = body["blobId"] as? String {
                store?.downloadFile(blobId: blobId)
            }
        case "previewFile":
            if let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            {
                store?.previewFile(blobId: blobId, filename: filename, mimeType: mimeType)
            }
        case "shareFile":
            // A long-press on a downloaded file / audio / video card — raise
            // the system share sheet on the materialised file.
            if let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            {
                store?.shareFile(blobId: blobId, filename: filename, mimeType: mimeType)
            }
        case "viewImage":
            // A tap on an image bubble — decode the cached blob and present the
            // full-screen zoomable viewer (images only; files use previewFile).
            // Name + mime ride along so the viewer's share sheet can hand over the
            // file under its real name.
            if let blobId = body["blobId"] as? String {
                store?.viewImage(
                    blobId: blobId,
                    filename: body["filename"] as? String ?? "",
                    mimeType: body["mimeType"] as? String ?? "")
            }
        case "audioToggle":
            if let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            {
                store?.audioToggle(blobId: blobId, filename: filename, mimeType: mimeType)
            }
        case "audioSeek":
            if let blobId = body["blobId"] as? String,
                let position = (body["position"] as? NSNumber)?.doubleValue
            {
                store?.audioSeek(blobId: blobId, position: position)
            }
        case "queryAudioState":
            if let blobId = body["blobId"] as? String {
                store?.queryAudioState(blobId: blobId)
            }
        case "playVideo":
            // A tap on a downloaded video tile — materialise and present the
            // native full-screen player.
            if let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            {
                store?.playVideo(blobId: blobId, filename: filename, mimeType: mimeType)
            }
        case "requestVideoPoster":
            if let id = (body["id"] as? NSNumber)?.intValue,
                let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            {
                store?.requestVideoPoster(
                    id: id, blobId: blobId, filename: filename, mimeType: mimeType)
            }
        case "retry":
            // The failed-bubble dot was tapped. The webview carries the payload
            // (so retry survives an eviction/relaunch that rebuilt the bubble);
            // reuse the msgId so the resend is idempotent.
            if let msgId = body["msgId"] as? String,
                let text = body["text"] as? String
            {
                store?.retrySend(
                    msgId: msgId, text: text, attachments: parseAttachments(body["attachments"]))
            }
        case "jumpVisible":
            jumpVisible = (body["visible"] as? Bool) ?? false
        case "outline":
            outlineHasMoreOlder = (body["hasMoreOlder"] as? Bool) ?? false
            outlineLoadingOlder = (body["loadingOlder"] as? Bool) ?? false
            if let entries = body["entries"],
                let data = try? JSONSerialization.data(withJSONObject: entries),
                let decoded = try? JSONDecoder().decode([OutlineEntry].self, from: data)
            {
                outline = decoded
                outlineFailed = false
            } else {
                // Keeping the previous rows would leave the sheet offering jumps
                // into a thread that has moved on — an empty index is honest.
                // But an empty index and a BROKEN one read identically to the
                // reader, and only one of them is their own fault, so the sheet
                // is told which happened.
                NSLog("baybo: outline decode failed")
                outline = []
                outlineFailed = true
            }
        case "outlineHere":
            outlineHereId = body["rowId"] as? String
        case "subagents":
            // OR, never assignment: the flag is a latch (see the property), and
            // the REST backstop below can also raise it for spawns that never
            // scrolled into the loaded window.
            if (body["present"] as? Bool) ?? false {
                subagentsPresent = true
            }
        case "runState":
            // The transcript's turn is/ isn't in flight — drives the composer's
            // send↔stop button on the store this webview currently targets.
            store?.setAgentRunning((body["running"] as? Bool) ?? false)
        case "htmlPreviewMaximized":
            htmlPreviewMaximized = (body["maximized"] as? Bool) ?? false
        case "openUrl":
            // Markdown links leave the app for the system browser — navigating
            // the transcript webview itself would replace the thread.
            if let raw = body["url"] as? String,
                let url = URL(string: raw),
                let scheme = url.scheme?.lowercased(),
                scheme == "http" || scheme == "https"
            {
                UIApplication.shared.open(url)
            }
        case "copy":
            // A user-bubble long-press. Native owns the write: a WKWebView
            // rejects `navigator.clipboard` outside a live gesture (the web
            // timer has none), and only native can fire the confirming haptic.
            if let text = body["text"] as? String, !text.isEmpty {
                UIPasteboard.general.string = text
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
            }
        case "log":
            let level = body["level"] as? String ?? "info"
            let message = body["message"] as? String ?? ""
            NSLog("baybo[web:%@]: %@", level, message)
        default:
            break
        }
    }

    /// Reconstruct `[AttachmentRef]` from the webview's wire attachments (the
    /// snake_case shape `userSent` posts) for a retry. Drops any entry missing
    /// its blob id / mime — a malformed item can't be re-enqueued.
    private func parseAttachments(_ raw: Any?) -> [AttachmentRef] {
        guard let items = raw as? [[String: Any]] else { return [] }
        return items.compactMap { item in
            guard let blobId = item["blob_id"] as? String,
                let mimeType = item["mime_type"] as? String
            else { return nil }
            return AttachmentRef(
                kind: attachmentKind(item["kind"] as? String),
                blobId: blobId,
                mimeType: mimeType,
                size: (item["size"] as? NSNumber)?.uint32Value ?? 0,
                filename: item["filename"] as? String)
        }
    }

    private func attachmentKind(_ raw: String?) -> AttachmentKind {
        switch raw {
        case "image": return .image
        case "audio": return .audio
        default: return .file
        }
    }
}
