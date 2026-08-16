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
    private var pending: [String] = []

    func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        // Only the shell (main frame) may drive the native bridge. Cards are
        // sandboxed subframes, and WKWebView injects the message handler into
        // EVERY frame — so without this guard a card's own JS could call the
        // native surface directly (`quickSetup` seeds and auto-sends an agent
        // prompt with no user tap; `cardAction`/`layout`/`delete` mutate the
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
            consecutiveDeaths = 0
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
        case "quickSetup":
            // Empty-board CTA: open a fresh chat and auto-send a `/deck …`
            // request.
            if let prompt = body["prompt"] as? String {
                AppStore.shared?.startCardDraft(prompt: prompt)
            }
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
    /// `TranscriptBridge.contentProcessDied`): a WebContent death under a
    /// VISIBLE deck leaves `ready` latched and every eval a silent no-op —
    /// bricked until app restart. Reload and let the fresh `ready` replay.
    /// The 30s window bounds a crash storm: three reloads, then quiet until
    /// the window lapses (`ready` re-arms the budget on a surviving load).
    private static let maxConsecutiveDeaths = 3
    private static let deathWindowSeconds: TimeInterval = 30
    private var consecutiveDeaths = 0
    private var lastDeathAt = Date.distantPast

    func contentProcessDied() {
        let now = Date()
        if now.timeIntervalSince(lastDeathAt) > Self.deathWindowSeconds {
            consecutiveDeaths = 0
        }
        lastDeathAt = now
        consecutiveDeaths += 1
        guard consecutiveDeaths <= Self.maxConsecutiveDeaths else {
            NSLog("baybo: deck web content process died again; giving up on reloads")
            return
        }
        guard let webView, let url = DeckHost.deckURL else { return }
        NSLog("baybo: deck web content process died; reloading")
        ready = false
        pending.removeAll()
        webView.load(URLRequest(url: url))
    }

    private func eval(_ fn: String, _ jsonPayload: String) {
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

    func setSetupInflight(_ active: Bool) {
        eval("setSetupInflight", active ? "true" : "false")
    }

    /// Ask the shell to collapse the maximized card (the native header's ✕).
    func restoreMaximized() {
        eval("restoreMaximized", "")
    }

    func setLanguage(_ code: String) {
        evalEncodable("setLanguage", code)
    }
}
