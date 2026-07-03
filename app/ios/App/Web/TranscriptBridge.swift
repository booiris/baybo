import Foundation
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
        guard ready, let webView else {
            pending.append(js)
            return
        }
        webView.evaluateJavaScript(js)
    }

    private func flushPending() {
        guard let webView else { return }
        for js in pending {
            webView.evaluateJavaScript(js)
        }
        pending.removeAll()
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
            ready = true
            deliverInit()
            flushPending()
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
        case "log":
            let level = body["level"] as? String ?? "info"
            let message = body["message"] as? String ?? ""
            NSLog("baybo[web:%@]: %@", level, message)
        default:
            break
        }
    }
}
