import Combine
import Foundation
import UIKit
import WebKit

/// Deck shell ⇄ native bridge (the transcript bridge's shape, deck-sized):
/// native drives `window.deckShell.*` evals, buffered until the shell posts
/// `ready`; the shell posts `{type: …}` bodies on the `deck` handler.
@MainActor
final class DeckBridge: NSObject, WKScriptMessageHandler {
    static let messageHandlerName = "deck"

    weak var webView: WKWebView?
    weak var store: DeckStore?

    private var ready = false
    /// Readable (as on `TranscriptBridge`) so a test can see what the bridge
    /// decided to send without standing up a WKWebView.
    private(set) var pending: [String] = []
    /// Live language pushes, held here rather than on the screen for the same
    /// reason `TranscriptBridge` does: the deck shell is PREWARMED at home and
    /// outlives every view, while `DeckContent.body` — where this used to be the
    /// only `.onChange` — has not necessarily ever run. Toggle the language
    /// before opening the Deck tab for the first time and the shell kept the
    /// language it was prewarmed with.
    private var languageWatch: AnyCancellable?

    override init() {
        super.init()
        // `dropFirst`: the current value already rides `DeckStore`'s init
        // payload; only CHANGES are news here.
        languageWatch = Lang.shared.$code.dropFirst().sink { [weak self] code in
            self?.setLanguage(code)
        }
    }

    func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        // Only the shell (main frame) may drive the native bridge. Cards are
        // sandboxed subframes, and WKWebView injects the message handler into
        // EVERY frame — so without this guard a card's own JS could call the
        // native surface directly (`cardAction`/`layout`/`delete` mutate the
        // deck), bypassing the port-mediated shell. Cards reach the shell over
        // their per-card MessagePort, never this handler.
        guard message.frameInfo.isMainFrame,
            message.name == Self.messageHandlerName,
            let body = message.body as? [String: Any],
            let type = body["type"] as? String
        else { return }
        MainActor.assumeIsolated {
            handle(type: type, body: body)
        }
    }

    private func handle(type: String, body: [String: Any]) {
        switch type {
        case "ready":
            WebLifecycleLog.note(.deck, "ready")
            ready = true
            store?.bridgeBecameReady()
            for js in pending {
                webView?.evaluateJavaScript(js)
            }
            pending.removeAll()
        case "refetch":
            store?.requestRefresh()
        case "requestBundle":
            if let cardId = body["cardId"] as? String {
                store?.requestBundle(cardId: cardId)
            }
        case "call":
            if let id = body["id"] as? String,
                let cardId = body["cardId"] as? String,
                let op = body["op"] as? String
            {
                store?.requestCall(id: id, cardId: cardId, op: op, params: body["params"])
            }
        case "pick":
            if let id = body["id"] as? String,
                let cardId = body["cardId"] as? String
            {
                store?.requestPick(id: id, cardId: cardId, accept: body["accept"] as? String)
            }
        case "share":
            if let blobId = body["blobId"] as? String {
                store?.requestShare(
                    blobId: blobId,
                    filename: body["filename"] as? String,
                    contentType: body["contentType"] as? String)
            }
        case "layout":
            if let entries = body["entries"] as? [[String: Any]] {
                store?.requestLayout(entries: entries)
            }
        case "cardAction":
            if let cardId = body["cardId"] as? String,
                let action = body["action"] as? String
            {
                store?.requestCardAction(cardId: cardId, action: action)
            }
        case "editMode":
            store?.editMode = body["active"] as? Bool ?? false
        case "maximize":
            // The shell entered/left a card's full-screen layout; native
            // hides the wordmark header while it's up.
            store?.setMaximized(body["active"] as? Bool ?? false)
        case "haptic":
            // The long-press reorder pickup.
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        case "log":
            let level = body["level"] as? String ?? "info"
            let text = body["message"] as? String ?? ""
            NSLog("deck-shell [%@] %@", level, text)
        default:
            break
        }
    }

    // MARK: native → web

    /// The transcript webview's crash-recovery twin (see
    /// `TranscriptBridge.contentProcessDied` and `CrashReloadBudget`): a
    /// WebContent death leaves `ready` latched and every eval a silent no-op.
    /// Reload and let the fresh `ready` replay; past the cap, park until a
    /// user-driven edge revives it.
    private var crashBudget = CrashReloadBudget()

    func contentProcessDied() {
        switch crashBudget.recordDeath() {
        case .reload:
            NSLog("baybo: deck web content process died; reloading")
            WebLifecycleLog.note(.deck, "content process died; reloading")
            reloadShell()
        case .park:
            NSLog("baybo: deck web content process died again; parking until revived")
            WebLifecycleLog.note(.deck, "content process died again; parked")
            ready = false
            pending.removeAll()
        }
    }

    /// The deck tab appeared, or the app returned to the foreground.
    func reviveIfParked() {
        guard crashBudget.revive() else { return }
        WebLifecycleLog.note(.deck, "revived parked page")
        reloadShell()
    }

    private func reloadShell() {
        guard let webView, let url = DeckHost.deckURL else { return }
        ready = false
        pending.removeAll()
        webView.load(URLRequest(url: url))
    }

    private func eval(_ fn: String, _ jsonPayload: String) {
        guard !crashBudget.parked else { return }
        let js = "window.deckShell.\(fn)(\(jsonPayload));"
        if ready {
            webView?.evaluateJavaScript(js)
        } else {
            pending.append(js)
        }
    }

    private func evalEncodable(_ fn: String, _ payload: some Encodable) {
        guard let data = try? JSONEncoder().encode(payload),
            let json = String(data: data, encoding: .utf8)
        else { return }
        eval(fn, json)
    }

    func deliverInit(_ payload: DeckStore.InitPayload) {
        evalEncodable("init", payload)
    }

    func deliverState(_ payload: DeckStore.StatePayload) {
        evalEncodable("deckState", payload)
    }

    func deliverCardData(cardId: String, seq: Int64, payload: String) {
        struct P: Encodable {
            let cardId: String
            let seq: Int64
            let payload: String
        }
        evalEncodable("cardData", P(cardId: cardId, seq: seq, payload: payload))
    }

    func deliverBundle(cardId: String, cardHtml: String) {
        struct P: Encodable {
            let cardId: String
            let cardHtml: String
        }
        evalEncodable("bundle", P(cardId: cardId, cardHtml: cardHtml))
    }

    /// `value` is the op result's raw JSON text (or nil on failure) — spliced
    /// verbatim so the card sees the JSON value, not a quoted string.
    func deliverCallResult(id: String, ok: Bool, valueJSON: String?, error: String?) {
        struct Head: Encodable {
            let id: String
            let ok: Bool
            let error: String?
        }
        guard let headData = try? JSONEncoder().encode(Head(id: id, ok: ok, error: error)),
            var head = String(data: headData, encoding: .utf8)
        else { return }
        if let valueJSON, ok {
            head.removeLast()  // strip trailing '}'
            head += ",\"value\":\(valueJSON)}"
        }
        eval("callResult", head)
    }

    /// `refJSON` is the blob ref's raw JSON (or nil on failure) — spliced
    /// verbatim so the card resolves with the object, not a quoted string.
    func deliverPickResult(id: String, ok: Bool, refJSON: String?, error: String?) {
        struct Head: Encodable {
            let id: String
            let ok: Bool
            let error: String?
        }
        guard let headData = try? JSONEncoder().encode(Head(id: id, ok: ok, error: error)),
            var head = String(data: headData, encoding: .utf8)
        else { return }
        if let refJSON, ok {
            head.removeLast()  // strip trailing '}'
            head += ",\"ref\":\(refJSON)}"
        }
        eval("pickResult", head)
    }

    func setEditMode(_ active: Bool) {
        eval("setEditMode", active ? "true" : "false")
    }

    /// Ask the shell to collapse the maximized card (the native header's ✕).
    func restoreMaximized() {
        eval("restoreMaximized", "")
    }

    func setLanguage(_ code: String) {
        evalEncodable("setLanguage", code)
    }
}
