import Foundation
import WebKit

/// Serves the bundled transcript (`App/Resources/transcript/`) over a custom
/// scheme. `file://` cannot host it: Vite emits `<script type="module"
/// crossorigin>`, and WebKit fetches module scripts with CORS — a request from
/// a file (opaque) origin is rejected, so the bundle never executes and the
/// bridge never comes up. A scheme handler answers with real HTTP semantics
/// (correct MIME + permissive CORS), which module loading accepts.
///
/// The DECK webview additionally enables a `/blob/<id>` route
/// (`blobRouteEnabled`) so a card iframe can display a blob it produced/was
/// handed via `deck.blobUrl` — the spike (docs/modules/deck.md §Blobs) proved
/// a sandboxed opaque-origin srcdoc iframe's `<img>` subresource reaches this
/// handler and the `img-src baybo-transcript:` CSP admits it.
@MainActor
final class TranscriptSchemeHandler: NSObject, WKURLSchemeHandler {
    static let scheme = "baybo-transcript"
    static let indexURL = URL(string: "\(scheme)://localhost/index.html")

    /// Only the deck webview serves blobs; the transcript webview leaves the
    /// route off so it can't be reached from a context that never needs it.
    private let blobRouteEnabled: Bool

    /// Live scheme tasks, by identity. An async blob serve MUST confirm its
    /// task is still live before touching it: WebKit tears a task down when the
    /// card is resized/removed mid-load, and messaging a stopped task crashes.
    /// Only touched on the main actor (WebKit drives start/stop there; the
    /// async serve is `@MainActor` too), so the set needs no extra locking.
    private var liveTasks: Set<ObjectIdentifier> = []

    /// Cap on a single blob served for display. Card imagery is small; a card
    /// that points `deck.blobUrl` at something huge fails to render rather than
    /// pulling it all into memory.
    private static let blobServeCap = 8 * 1024 * 1024

    init(blobRouteEnabled: Bool = false) {
        self.blobRouteEnabled = blobRouteEnabled
        super.init()
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: WKURLSchemeTask) {
        guard let url = urlSchemeTask.request.url else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }
        // Blob route (deck only), matched AHEAD of the bundle-file fallthrough.
        if blobRouteEnabled, url.path.hasPrefix("/blob/") {
            startBlob(url: url, task: urlSchemeTask)
            return
        }
        guard let root = Bundle.main.url(forResource: "transcript", withExtension: nil) else {
            NSLog("baybo: transcript bundle missing from app resources")
            urlSchemeTask.didFailWithError(URLError(.fileDoesNotExist))
            return
        }
        var path = url.path
        if path.isEmpty || path == "/" {
            path = "/index.html"
        }
        let file = root.appendingPathComponent(String(path.dropFirst())).standardizedFileURL
        // Stay inside the bundle dir (nothing legitimate uses "..").
        guard file.path.hasPrefix(root.standardizedFileURL.path),
            let data = try? Data(contentsOf: file)
        else {
            NSLog("baybo: transcript asset not found: %@", path)
            urlSchemeTask.didFailWithError(URLError(.fileDoesNotExist))
            return
        }
        let headers = [
            "Content-Type": Self.mimeType(for: file.pathExtension),
            "Access-Control-Allow-Origin": "*",
            "Content-Length": String(data.count),
        ]
        guard
            let response = HTTPURLResponse(
                url: url, statusCode: 200, httpVersion: "HTTP/1.1", headerFields: headers)
        else {
            urlSchemeTask.didFailWithError(URLError(.badServerResponse))
            return
        }
        urlSchemeTask.didReceive(response)
        urlSchemeTask.didReceive(data)
        urlSchemeTask.didFinish()
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: WKURLSchemeTask) {
        // Marks the task dead so an in-flight async serve drops it (the crash
        // guard). Idempotent with the serve's own removal.
        liveTasks.remove(ObjectIdentifier(urlSchemeTask))
    }

    // MARK: - Blob route (deck)

    private func startBlob(url: URL, task: WKURLSchemeTask) {
        let blobId = String(url.path.dropFirst("/blob/".count))
        let contentType =
            URLComponents(url: url, resolvingAgainstBaseURL: false)?
            .queryItems?.first(where: { $0.name == "ct" })?.value
            ?? "application/octet-stream"
        let id = ObjectIdentifier(task)
        liveTasks.insert(id)
        Task { await serveBlob(url: url, task: task, id: id, blobId: blobId, contentType: contentType) }
    }

    private func serveBlob(
        url: URL, task: WKURLSchemeTask, id: ObjectIdentifier, blobId: String, contentType: String
    ) async {
        // The core owns the serve: id-shape validation, cache-first read (no
        // binding — a cached image renders offline/unbound), leg-bound download
        // on a miss, and the display cap (an over-cap cached blob is refused by
        // its stat, never read). We only adapt the outcome to WebKit.
        let outcome = await Baybo.client.blobBytesForDisplay(
            blobId: blobId, maxBytes: UInt64(Self.blobServeCap))
        // Back on the main actor after the await: confirm-and-consume the task
        // in one step. `nil` means `stop` already fired — do not touch it.
        guard liveTasks.remove(id) != nil else { return }
        switch outcome {
        case .bytes(let data):
            let headers = [
                "Content-Type": contentType,
                "Access-Control-Allow-Origin": "*",
                "Content-Length": String(data.count),
            ]
            guard
                let response = HTTPURLResponse(
                    url: url, statusCode: 200, httpVersion: "HTTP/1.1", headerFields: headers)
            else {
                task.didFailWithError(URLError(.badServerResponse))
                return
            }
            task.didReceive(response)
            task.didReceive(data)
            task.didFinish()
        case .overCap:
            task.didFailWithError(URLError(.dataLengthExceedsMaximum))
        case .badId:
            task.didFailWithError(URLError(.badURL))
        case .notFound:
            task.didFailWithError(URLError(.resourceUnavailable))
        }
    }

    private static func mimeType(for ext: String) -> String {
        switch ext.lowercased() {
        case "html": return "text/html; charset=utf-8"
        case "js", "mjs": return "text/javascript"
        case "css": return "text/css"
        case "woff2": return "font/woff2"
        case "woff": return "font/woff"
        case "svg": return "image/svg+xml"
        case "png": return "image/png"
        case "jpg", "jpeg": return "image/jpeg"
        case "json": return "application/json"
        default: return "application/octet-stream"
        }
    }
}
