import Foundation
import UIKit
import WebKit

/// The card page ⇄ native bridge.
///
/// It answers on the **same** `baybo` handler name the transcript uses, and
/// that is deliberate: the page imports `Markdown`, the attachment cards and
/// `blobObjectUrl` unchanged, and all of those post through that channel. A
/// second handler name would have meant forking every one of them.
///
/// A warm issue host and the transcript never coexist on one webview — each
/// has its own `WKUserContentController` — so sharing the name costs nothing
/// and buys the whole shared half of the web bundle.
@MainActor
final class IssueBridge: NSObject, WKScriptMessageHandler, WebMediaSink {
    /// Same name as the transcript's. See the type doc.
    static let messageHandlerName = TranscriptBridge.messageHandlerName

    weak var webView: WKWebView?
    private(set) weak var store: IssueStore?

    private var ready = false
    private var pending: [String] = []
    private var lastBottomInset = Int.min
    private var composerTop: CGFloat?
    private var targetId: String?

    func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        // Main frame only. The card body renders agent-authored markdown, and
        // WKWebView injects a message handler into EVERY frame — so without
        // this an iframe smuggled into a description would reach the native
        // surface directly. The transcript's bridge draws the same line.
        guard message.frameInfo.isMainFrame,
            message.name == Self.messageHandlerName,
            let body = message.body as? [String: Any],
            let type = body["type"] as? String
        else { return }
        MainActor.assumeIsolated { handle(type: type, body: body) }
    }

    private func handle(type: String, body: [String: Any]) {
        // The attachment cards' messages first, through the shared dispatch —
        // whatever it consumes never reaches this page's own switch, and what
        // it does not consume is this page's alone.
        if let store, isCurrent(body),
            WebMediaDispatch.handle(type: type, body: body, target: store)
        {
            return
        }

        switch type {
        case "issueReady", "ready":
            consecutiveDeaths = 0
            ready = true
            let hadPending = !pending.isEmpty
            for js in pending { webView?.evaluateJavaScript(js) }
            pending.removeAll()
            if !hadPending { replayTarget() }
        case "issueRendered":
            guard isCurrent(body) else { return }
            store?.markRendered()
        case "openIssue":
            guard isCurrent(body) else { return }
            if let number = (body["number"] as? NSNumber)?.int64Value, let store {
                AppStore.shared?.openProjectIssue(project: store.projectId, number: number)
            }
        case "openRun":
            guard isCurrent(body) else { return }
            if let attempt = (body["attempt"] as? NSNumber)?.int64Value {
                store?.openRunRequest = attempt
            }
        case "pick":
            guard isCurrent(body) else { return }
            if let field = body["field"] as? String {
                store?.pickRequest = field
            }
        case "generatedFace":
            guard isCurrent(body) else { return }
            if let agentId = body["agentId"] as? String,
                let png = body["pngBase64"] as? String
            {
                store?.storeGeneratedFace(agentId: agentId, pngBase64: png)
            }
        case "activityAtBottom":
            guard isCurrent(body) else { return }
            store?.setAtBottom(body["atBottom"] as? Bool ?? true)
        case "issueState":
            guard isCurrent(body),
                let scrollTop = (body["scrollTop"] as? NSNumber)?.doubleValue,
                let folds = body["folds"] as? [String: Bool]
            else { return }
            store?.rememberPageState(scrollTop: scrollTop, folds: folds)
        case "openUrl":
            guard isCurrent(body) else { return }
            if let url = body["url"] as? String, let parsed = URL(string: url) {
                UIApplication.shared.open(parsed)
            }
        case "copy":
            guard isCurrent(body) else { return }
            // Native owns the write, as it does for the transcript: a
            // WKWebView refuses `navigator.clipboard` outside a live gesture,
            // and only native can fire the confirming haptic.
            if let text = body["text"] as? String, !text.isEmpty {
                UIPasteboard.general.string = text
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
            }
        case "log":
            NSLog(
                "issue-page [%@] %@", body["level"] as? String ?? "info",
                body["message"] as? String ?? "")
        default:
            break
        }
    }

    private func isCurrent(_ body: [String: Any]) -> Bool {
        guard let targetId else { return false }
        return body["targetId"] as? String == targetId
    }

    // Opening a run sheet and presenting a picker are PRESENTATION, and a
    // bridge that reached for the view hierarchy would be a second place
    // navigation happens — so those two, and the page's at-bottom report, are
    // raised to the screen rather than handled here.
    //
    // Raised as STATE ON THE STORE (`pickRequest` / `openRunRequest` /
    // `setAtBottom`), never as `onPick` / `onOpenRun` / `onActivityAtBottom`
    // closures the screen installs — which is what they were until 2026-08-26.
    // A closure written inside a `View`'s body captures the whole view struct,
    // `@StateObject` storage included, so storing one here closed the cycle
    // `IssueHost → bridge → closure → view → host` and made the card page
    // immortal: every card ever opened kept an invalidation observer refetching
    // behind it. A warm host makes that invariant stricter: `store` is weak, so
    // retargeting cannot retain any prior visit whatever the screen does.

    // MARK: - native → web

    /// The transcript and deck bridges' crash-recovery twin: a WebContent
    /// death under a VISIBLE page leaves `ready` latched and every eval a
    /// silent no-op — bricked until the screen is left and re-entered. Reload
    /// and let the fresh `ready` replay. The 30s window bounds a crash storm:
    /// three reloads, then quiet until it lapses.
    private static let maxConsecutiveDeaths = 3
    private static let deathWindowSeconds: TimeInterval = 30
    private var consecutiveDeaths = 0
    private var lastDeathAt = Date.distantPast

    func retarget(to next: IssueStore, targetId: String) {
        if self.targetId == targetId, store === next { return }
        let previous = store
        store?.detach(self)
        pending.removeAll()
        self.targetId = targetId
        store = next
        composerTop = next.composerTop
        lastBottomInset = Int.min
        next.attach(self)
        deliverInit(capturing: previous)
        next.redeliver()
        pushBottomInset()
    }

    func clearTarget(_ targetId: String) {
        guard self.targetId == targetId else { return }
        store?.detach(self)
        store = nil
        self.targetId = nil
        composerTop = nil
        pending.removeAll()
    }

    func teardown() {
        if let targetId { clearTarget(targetId) }
        ready = false
        pending.removeAll()
        webView = nil
    }

    private func replayTarget() {
        guard store != nil, targetId != nil else { return }
        deliverInit()
        store?.redeliver()
        lastBottomInset = Int.min
        pushBottomInset()
    }

    /// Reload the page from scratch — the third step of `IssueStore.resync`.
    ///
    /// The same load `IssueHost.init` performs, so every piece of in-memory
    /// web state dies with the document: the rendered card, the scroll
    /// position, an open description editor. The fresh `issueReady` replays
    /// whatever the refetch has landed by then, and buffers what it has not.
    func rebuild() {
        guard let webView, let url = IssueHost.issueURL else { return }
        ready = false
        pending.removeAll()
        lastBottomInset = Int.min
        webView.load(URLRequest(url: url))
    }

    func contentProcessDied() {
        let now = Date()
        if now.timeIntervalSince(lastDeathAt) > Self.deathWindowSeconds { consecutiveDeaths = 0 }
        lastDeathAt = now
        consecutiveDeaths += 1
        guard consecutiveDeaths <= Self.maxConsecutiveDeaths else {
            NSLog("baybo: issue web content process died again; giving up on reloads")
            return
        }
        guard let webView, let url = IssueHost.issueURL else { return }
        NSLog("baybo: issue web content process died; reloading")
        ready = false
        pending.removeAll()
        webView.load(URLRequest(url: url))
    }

    private func eval(_ target: String, _ fn: String, _ jsonPayload: String) {
        let js = "window.\(target).\(fn)(\(jsonPayload));"
        if ready {
            webView?.evaluateJavaScript(js)
        } else {
            pending.append(js)
        }
    }

    /// The card page's own calls.
    private func page(_ fn: String, _ jsonPayload: String) { eval("issuePage", fn, jsonPayload) }
    /// The shared bundle's calls — the attachment cards listen on `window.baybo`
    /// because they are literally the transcript's components.
    private func shared(_ fn: String, _ jsonPayload: String) { eval("baybo", fn, jsonPayload) }

    private func deliverInit(capturing previous: IssueStore? = nil) {
        guard let store, let targetId else { return }
        var payload: [String: Any] = [
            "language": Lang.shared.current.lproj,
            "projectId": store.projectId,
            "number": store.number,
            "targetId": targetId,
            "bottomInset": store.bottomInset,
        ]
        if let state = store.pageState {
            payload["restoredState"] = [
                "scrollTop": state.scrollTop,
                "folds": state.folds,
            ]
        }
        let json = Self.jsonObject(payload)
        guard let previous, ready, let webView else {
            page("init", json)
            return
        }
        let js = """
            JSON.stringify((function() {
              const state = window.issuePage.snapshotState();
              window.issuePage.init(\(json));
              return state;
            })());
            """
        webView.evaluateJavaScript(js) { [weak previous] value, error in
            MainActor.assumeIsolated {
                if let error {
                    NSLog("baybo: issue retarget failed: %@", error.localizedDescription)
                    return
                }
                guard let previous, let json = value as? String, json != "null",
                    let data = json.data(using: .utf8),
                    let state = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                    let scrollTop = (state["scrollTop"] as? NSNumber)?.doubleValue,
                    let folds = state["folds"] as? [String: Bool]
                else { return }
                previous.rememberPageState(scrollTop: scrollTop, folds: folds)
            }
        }
    }

    func deliver(
        issue: IssueInfo, eventsJson: String, runs: [IssueRunInfo],
        people: [String: IssuePerson], children: [IssueInfo], firstUnread: String?
    ) {
        page(
            "deliver",
            Self.payload(
                issue: issue, eventsJson: eventsJson, runs: runs, people: people,
                children: children, firstUnread: firstUnread))
    }

    /// Everything the page draws, as the JSON it is handed.
    ///
    /// Split out from `deliver` so the SPLICE below has a test: it is string
    /// surgery on an encoder's output, and getting it wrong produces valid
    /// JSON with a field quietly missing rather than anything that fails.
    static func payload(
        issue: IssueInfo, eventsJson: String, runs: [IssueRunInfo],
        people: [String: IssuePerson], children: [IssueInfo], firstUnread: String?
    ) -> String {
        var payload: [String: Any] = [
            "issue": IssueWire.card(issue),
            "runs": runs.map(IssueWire.run(_:)),
            "people": people.mapValues(IssueWire.person(_:)),
            "children": children.map(IssueWire.child(_:)),
        ]
        // Omitted rather than sent as null when there is nothing new: the page
        // latches the first boundary it is given and never clears it, so a
        // `null` arriving after the card is stamped read must be indistinguish-
        // able from silence.
        if let firstUnread { payload["firstUnread"] = firstUnread }
        // The timeline is SPLICED in as the gateway's own bytes rather than
        // re-encoded: its only consumer is the page, and a Swift mirror of it
        // would be a third place every new event kind has to be taught about.
        var json = jsonObject(payload)
        if json.hasSuffix("}"), let items = itemsArray(eventsJson) {
            json.removeLast()
            json += ",\"events\":\(items)}"
        }
        return json
    }

    /// The `items` array out of the timeline envelope, verbatim. Nil when the
    /// envelope is not the shape it claims — the page then simply renders no
    /// Activity, which is better than a card that fails to open.
    private static func itemsArray(_ envelope: String) -> String? {
        guard let data = envelope.data(using: .utf8),
            let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let items = root["items"],
            let reencoded = try? JSONSerialization.data(withJSONObject: items),
            let json = String(data: reencoded, encoding: .utf8)
        else { return nil }
        return json
    }

    /// The dock's top in `.global` coordinates. Convert against this
    /// webview's WINDOW, not `UIScreen`: the latter over-insets an iPad window
    /// and mixes two coordinate spaces even on a full-screen phone.
    func setComposerTop(_ minYInWindow: CGFloat) {
        composerTop = minYInWindow
        pushBottomInset()
    }

    private func pushBottomInset() {
        guard let composerTop, let window = webView?.window else { return }
        let px = max(0, Int((window.bounds.height - composerTop).rounded()))
        guard px != lastBottomInset else { return }
        lastBottomInset = px
        store?.rememberBottomInset(px)
        page("setBottomInset", String(px))
    }

    func setLanguage(_ code: String) {
        page("setLanguage", Self.jsonLiteral(code))
        shared("setLanguage", Self.jsonLiteral(code))
    }

    func jumpToLatest() {
        page("jumpToLatest", "")
    }

    // MARK: - WebMediaSink
    //
    // The attachment cards listen on `window.baybo` — they ARE the transcript's
    // components — so these four go to the shared target, not the page's.

    func blobResult(id: Int, dataBase64: String?, mimeType: String, error: String?) {
        shared(
            "blobResult",
            Self.jsonObject([
                "id": id, "dataBase64": dataBase64 as Any, "mimeType": mimeType,
                "error": error as Any,
            ]))
    }

    func fileState(
        blobId: String, state: String, loaded: UInt64?, total: UInt64?, error: String?
    ) {
        var payload: [String: Any] = ["blobId": blobId, "state": state]
        if let loaded { payload["loaded"] = loaded }
        if let total { payload["total"] = total }
        if let error { payload["error"] = error }
        shared("fileState", Self.jsonObject(payload))
    }

    func audioState(blobId: String, state: String, position: Double, duration: Double) {
        shared(
            "audioState",
            Self.jsonObject([
                "blobId": blobId, "state": state, "position": position, "duration": duration,
            ]))
    }

    func videoPoster(
        id: Int, dataBase64: String?, width: Int, height: Int, durationMs: Int, error: String?
    ) {
        shared(
            "videoPoster",
            Self.jsonObject([
                "id": id, "dataBase64": dataBase64 as Any, "width": width, "height": height,
                "durationMs": durationMs, "error": error as Any,
            ]))
    }

    private static func jsonObject(_ payload: [String: Any]) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
            let json = String(data: data, encoding: .utf8)
        else { return "{}" }
        return json
    }

    private static func jsonLiteral(_ value: String) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: [value]),
            let json = String(data: data, encoding: .utf8)
        else { return "\"\"" }
        return String(json.dropFirst().dropLast())
    }
}
