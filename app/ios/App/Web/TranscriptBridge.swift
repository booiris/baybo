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
final class TranscriptBridge: NSObject, ObservableObject {
    static let messageHandlerName = "baybo"

    private weak var store: ChatStore?
    weak var webView: WKWebView?
    private var ready = false
    private var pending: [String] = []
    private var lastBottomInset = Int.min
    private var composerTop: CGFloat?
    /// Mirror of the web transcript's `showJump` state — drives the native
    /// glass jump-to-latest button above the composer.
    @Published private(set) var jumpVisible = false

    init(store: ChatStore) {
        self.store = store
        super.init()
        store.bridge = self
    }

    // MARK: - Native → web

    func pushFrame(_ frameJson: String) {
        call("pushFrame", jsonLiteral(frameJson))
    }

    func setConnEpoch(_ epoch: Int) {
        call("setConnEpoch", String(epoch))
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

    func imageResult(id: Int, dataBase64: String?, mimeType: String, error: String?) {
        let payload: [String: Any] = [
            "id": id,
            "dataBase64": dataBase64 as Any,
            "mimeType": mimeType,
            "error": error as Any,
        ]
        call("imageResult", jsonObjectLiteral(payload))
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
        let restored = UserDefaults.standard.string(forKey: ChatDefaults.transcriptState)
        // The in-app language override (falls back to the device language) —
        // the same source the native chrome renders from, so the two can't
        // diverge.
        let language = Lang.shared.code
        // restoredState is already a JSON object string — splice it in raw.
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
            guard let body = message.body as? [String: Any],
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
            // Fresh page (first load or silent reload after a web-process
            // kill): its inset var is at the 0px default — replay the inset
            // past the dedup. Also the cold-launch delivery: the composer
            // geometry usually fires before the webview joins a window, and
            // by page-load time it has one.
            lastBottomInset = Int.min
            pushBottomInset()
            // Same fresh-page reasoning: the new page opens pinned to the
            // newest edge, so a jetsam reload must not strand a stale button.
            jumpVisible = false
        case "ordinal":
            let ordinal = (body["lastOrdinal"] as? NSNumber)?.int64Value
            store?.ordinalAdvanced(ordinal)
        case "persist":
            if let state = body["state"],
                let data = try? JSONSerialization.data(withJSONObject: state),
                let json = String(data: data, encoding: .utf8)
            {
                UserDefaults.standard.set(json, forKey: ChatDefaults.transcriptState)
            }
        case "fetchHistory":
            let before = (body["beforeOrdinal"] as? NSNumber)?.int64Value
            let limit = (body["limit"] as? NSNumber)?.uint32Value ?? 50
            store?.fetchHistory(beforeOrdinal: before, limit: limit)
        case "requestImage":
            if let id = (body["id"] as? NSNumber)?.intValue,
                let blobId = body["blobId"] as? String
            {
                store?.requestImage(id: id, blobId: blobId)
            }
        case "jumpVisible":
            jumpVisible = (body["visible"] as? Bool) ?? false
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
        case "log":
            let level = body["level"] as? String ?? "info"
            let message = body["message"] as? String ?? ""
            NSLog("baybo[web:%@]: %@", level, message)
        default:
            break
        }
    }
}
