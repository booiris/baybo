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
/// The two bridges never coexist on one webview — each page is its own
/// `WKWebView` with its own `WKUserContentController` — so sharing the name
/// costs nothing and buys the whole shared half of the web bundle.
@MainActor
final class IssueBridge: NSObject, WKScriptMessageHandler, WebMediaSink {
    /// Same name as the transcript's. See the type doc.
    static let messageHandlerName = TranscriptBridge.messageHandlerName

    weak var webView: WKWebView?
    weak var store: IssueStore?

    private var ready = false
    private var pending: [String] = []
    private var lastBottomInset = Int.min

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
        if let store, WebMediaDispatch.handle(type: type, body: body, target: store) { return }

        switch type {
        case "issueReady", "ready":
            consecutiveDeaths = 0
            ready = true
            for js in pending { webView?.evaluateJavaScript(js) }
            pending.removeAll()
            // The page's tree is brand new — after a crash reload it holds
            // nothing at all — so hand it everything again rather than waiting
            // for the next invalidation.
            store?.redeliver()
        case "issueRendered":
            store?.markRendered()
        case "descriptionDone":
            if let text = body["text"] as? String {
                store?.setDescription(text)
                store?.editing = false
            }
        case "openIssue":
            if let number = (body["number"] as? NSNumber)?.int64Value, let store {
                AppStore.shared?.openProjectIssue(project: store.projectId, number: number)
            }
        case "openRun":
            if let attempt = (body["attempt"] as? NSNumber)?.int64Value {
                onOpenRun?(attempt)
            }
        case "pick":
            if let field = body["field"] as? String {
                onPick?(field)
            }
        case "activityAtBottom":
            onActivityAtBottom?(body["atBottom"] as? Bool ?? true)
        case "openUrl":
            if let url = body["url"] as? String, let parsed = URL(string: url) {
                UIApplication.shared.open(parsed)
            }
        case "copy":
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

    /// Raised to the screen rather than handled here: opening a run sheet and
    /// presenting a picker are presentation, and a bridge that reached for the
    /// view hierarchy would be a second place navigation happens.
    var onOpenRun: ((Int64) -> Void)?
    var onPick: ((String) -> Void)?
    var onActivityAtBottom: ((Bool) -> Void)?

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

    func deliverInit(language: String, bottomInset: Int) {
        guard let store else { return }
        let payload: [String: Any] = [
            "language": language,
            "projectId": store.projectId,
            "number": store.number,
            "bottomInset": bottomInset,
        ]
        page("init", Self.jsonObject(payload))
    }

    func deliver(
        issue: IssueInfo, eventsJson: String, runs: [IssueRunInfo],
        handles: [String: String], children: [IssueInfo]
    ) {
        let payload: [String: Any] = [
            "issue": IssueWire.card(issue),
            "runs": runs.map(IssueWire.run(_:)),
            "handles": handles,
            "children": children.map(IssueWire.child(_:)),
        ]
        // The timeline is SPLICED in as the gateway's own bytes rather than
        // re-encoded: its only consumer is the page, and a Swift mirror of it
        // would be a third place every new event kind has to be taught about.
        var json = Self.jsonObject(payload)
        if json.hasSuffix("}"), let items = Self.itemsArray(eventsJson) {
            json.removeLast()
            json += ",\"events\":\(items)}"
        }
        page("deliver", json)
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

    func setBottomInset(_ px: Int) {
        guard px != lastBottomInset else { return }
        lastBottomInset = px
        page("setBottomInset", String(px))
    }

    func setLanguage(_ code: String) {
        page("setLanguage", Self.jsonLiteral(code))
        shared("setLanguage", Self.jsonLiteral(code))
    }

    func setEditing(_ active: Bool) {
        page("setEditing", active ? "true" : "false")
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
