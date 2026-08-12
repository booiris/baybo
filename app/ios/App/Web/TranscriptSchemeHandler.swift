import Foundation
import WebKit

/// Serves the bundled transcript (`App/Resources/transcript/`) over a custom
/// scheme. `file://` cannot host it: Vite emits `<script type="module"
/// crossorigin>`, and WebKit fetches module scripts with CORS — a request from
/// a file (opaque) origin is rejected, so the bundle never executes and the
/// bridge never comes up. A scheme handler answers with real HTTP semantics
/// (correct MIME + permissive CORS), which module loading accepts.
///
/// Each host opts into at most one dynamic route in addition to the bundle:
/// Deck serves `/blob/<id>` imagery, while the transcript serves agent-authored
/// `/html-preview/<id>` documents under a strict response CSP.
@MainActor
final class TranscriptSchemeHandler: NSObject, WKURLSchemeHandler {
    static let scheme = "baybo-transcript"
    static let indexURL = URL(string: "\(scheme)://localhost/index.html")

    enum DynamicRoute: Equatable {
        case deckBlob
        case htmlPreview
    }

    private static let deckBlobPathPrefix = "/blob/"
    private static let htmlPreviewPathPrefix = "/html-preview/"
    private static let deckBlobServeCap = 8 * 1024 * 1024
    private static let htmlPreviewServeCap = 16 * 1024 * 1024
    private static let htmlPreviewCSP =
        "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none';"
    private static let htmlPreviewPermissionsPolicy =
        "accelerometer=(), autoplay=(), camera=(), display-capture=(), encrypted-media=(), fullscreen=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), midi=(), payment=(), picture-in-picture=(), publickey-credentials-get=(), screen-wake-lock=(), usb=(), web-share=(), xr-spatial-tracking=()"
    private static let htmlPreviewFailureDocument =
        #"<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><style>html,body{height:100%;margin:0}body{display:grid;place-items:center;padding:24px;box-sizing:border-box;color:#6b6b6b;background:#fff;font:14px ui-monospace,SFMono-Regular,monospace;text-align:center}</style><body>HTML preview unavailable. Use the reload button to try again.</body>"#

    #if DEBUG
        /// The page `-baybo-demo-html` (see `DemoFrames`) points its marker at.
        /// A demo session has no leg, so a real blob read could only ever answer
        /// `notFound` and the reader would get the failure document. Gated on
        /// BOTH the exact id and the launch argument, so nothing reaches it
        /// without having asked. The bytes obey the same response CSP as any
        /// agent-authored page — inline style, no network, no images.
        static let demoHtmlBlobId = "sha256:" + String(repeating: "de", count: 32) + ".d0"
        private static let demoHtmlArg = "-baybo-demo-html"
        /// Deliberately DARK, and it has to stay that way: the fixture's job is
        /// to be a page that is not the transcript's own paper. It was white,
        /// and a white agent page is exactly the one that hides how the frame
        /// around it is painted — the full-screen preview reserved the home
        /// indicator in `--color-paper` and nothing in any screenshot of this
        /// document could show it. `HtmlPreviewUITests` now samples those
        /// pixels, so a white page here would take that assertion with it.
        private static let demoHtmlDocument = #"""
            <!doctype html><html lang="en"><head><meta charset="utf-8">
            <meta name="viewport" content="width=device-width,initial-scale=1">
            <style>
              body{margin:0;padding:18px;font:14px/1.5 -apple-system,system-ui,sans-serif;color:#f2f2f7;background:#12141a}
              h1{margin:0 0 2px;font-size:17px}p.sub{margin:0 0 18px;color:#9a9aa2;font-size:12px}
              .bars{display:flex;align-items:flex-end;gap:10px;height:150px}
              .bar{flex:1;background:linear-gradient(180deg,#5b7cfa,#8b5cf6);border-radius:6px 6px 0 0;position:relative}
              .bar span{position:absolute;bottom:-18px;left:0;right:0;text-align:center;font-size:11px;color:#9a9aa2}
              .row{display:flex;gap:12px;margin-top:32px}
              .kpi{flex:1;border:1px solid #2a2d36;border-radius:10px;padding:10px 12px}
              .kpi b{display:block;font-size:20px}.kpi em{font-style:normal;font-size:11px;color:#9a9aa2}
            </style></head><body>
            <h1>Quarterly sales</h1><p class="sub">2026 · thousands</p>
            <div class="bars">
              <div class="bar" style="height:45%"><span>Q1</span></div>
              <div class="bar" style="height:70%"><span>Q2</span></div>
              <div class="bar" style="height:58%"><span>Q3</span></div>
              <div class="bar" style="height:92%"><span>Q4</span></div>
            </div>
            <div class="row">
              <div class="kpi"><b>412</b><em>full year</em></div>
              <div class="kpi"><b>+18%</b><em>year over year</em></div>
              <div class="kpi"><b>103</b><em>quarter mean</em></div>
            </div>
            </body></html>
            """#
    #endif

    private let dynamicRoute: DynamicRoute

    /// Live scheme tasks, by identity. An async blob serve MUST confirm its
    /// task is still live before touching it: WebKit tears a task down when the
    /// dynamic resource is removed or reloaded mid-load, and messaging a
    /// stopped task crashes.
    /// Only touched on the main actor (WebKit drives start/stop there; the
    /// async serve is `@MainActor` too), so the set needs no extra locking.
    private var liveTasks: Set<ObjectIdentifier> = []

    init(dynamicRoute: DynamicRoute) {
        self.dynamicRoute = dynamicRoute
        super.init()
    }

    static func permitsNavigation(to url: URL, isMainFrame: Bool) -> Bool {
        guard url.scheme?.lowercased() == scheme,
            url.host?.lowercased() == "localhost"
        else {
            return false
        }
        if isMainFrame {
            return url.path.isEmpty || url.path == "/" || url.path == "/index.html"
        }
        return url.path.hasPrefix(htmlPreviewPathPrefix)
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: WKURLSchemeTask) {
        guard let url = urlSchemeTask.request.url else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }
        if dynamicRoute == .deckBlob, url.path.hasPrefix(Self.deckBlobPathPrefix) {
            startDeckBlob(url: url, task: urlSchemeTask)
            return
        }
        if dynamicRoute == .htmlPreview, url.path.hasPrefix(Self.htmlPreviewPathPrefix) {
            startHtmlPreview(url: url, task: urlSchemeTask)
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

    // MARK: - Dynamic blob routes

    private enum BlobResponse {
        case deck(contentType: String)
        case htmlPreview
    }

    private func startDeckBlob(url: URL, task: WKURLSchemeTask) {
        let blobId = String(url.path.dropFirst(Self.deckBlobPathPrefix.count))
        let contentType =
            URLComponents(url: url, resolvingAgainstBaseURL: false)?
            .queryItems?.first(where: { $0.name == "ct" })?.value
            ?? "application/octet-stream"
        startBlob(
            url: url, task: task, blobId: blobId,
            maxBytes: UInt64(Self.deckBlobServeCap),
            response: .deck(contentType: contentType))
    }

    private func startHtmlPreview(url: URL, task: WKURLSchemeTask) {
        let blobId = String(url.path.dropFirst(Self.htmlPreviewPathPrefix.count))
        #if DEBUG
            if blobId == Self.demoHtmlBlobId,
                ProcessInfo.processInfo.arguments.contains(Self.demoHtmlArg)
            {
                finish(
                    task: task, url: url, status: 200,
                    data: Data(Self.demoHtmlDocument.utf8), response: .htmlPreview)
                return
            }
        #endif
        startBlob(
            url: url, task: task, blobId: blobId,
            maxBytes: UInt64(Self.htmlPreviewServeCap), response: .htmlPreview)
    }

    private func startBlob(
        url: URL, task: WKURLSchemeTask, blobId: String, maxBytes: UInt64,
        response: BlobResponse
    ) {
        let id = ObjectIdentifier(task)
        liveTasks.insert(id)
        Task {
            await serveBlob(
                url: url, task: task, id: id, blobId: blobId, maxBytes: maxBytes,
                response: response)
        }
    }

    private func serveBlob(
        url: URL, task: WKURLSchemeTask, id: ObjectIdentifier, blobId: String,
        maxBytes: UInt64, response: BlobResponse
    ) async {
        // The core owns the serve: id-shape validation, cache-first read (no
        // binding — a cached display blob renders offline/unbound), leg-bound
        // download on a miss, and the display cap (an over-cap cached blob is
        // refused by its stat, never read). We only adapt the outcome to WebKit.
        let outcome = await Baybo.client.blobBytesForDisplay(
            blobId: blobId, maxBytes: maxBytes)
        // Back on the main actor after the await: confirm-and-consume the task
        // in one step. `nil` means `stop` already fired — do not touch it.
        guard liveTasks.remove(id) != nil else { return }
        switch outcome {
        case .bytes(let data):
            finish(task: task, url: url, status: 200, data: data, response: response)
        case .overCap:
            fail(
                task: task, url: url, status: 413, error: .dataLengthExceedsMaximum,
                response: response)
        case .badId:
            fail(task: task, url: url, status: 400, error: .badURL, response: response)
        case .notFound:
            fail(
                task: task, url: url, status: 503, error: .resourceUnavailable,
                response: response)
        }
    }

    private func fail(
        task: WKURLSchemeTask, url: URL, status: Int, error: URLError.Code,
        response: BlobResponse
    ) {
        switch response {
        case .deck:
            task.didFailWithError(URLError(error))
        case .htmlPreview:
            finish(
                task: task, url: url, status: status,
                data: Data(Self.htmlPreviewFailureDocument.utf8), response: response)
        }
    }

    private func finish(
        task: WKURLSchemeTask, url: URL, status: Int, data: Data, response: BlobResponse
    ) {
        var headers = [
            "Access-Control-Allow-Origin": "*",
            "Content-Length": String(data.count),
        ]
        switch response {
        case .deck(let contentType):
            headers["Content-Type"] = contentType
        case .htmlPreview:
            headers["Content-Type"] = "text/html; charset=utf-8"
            headers["Content-Security-Policy"] = Self.htmlPreviewCSP
            headers["Permissions-Policy"] = Self.htmlPreviewPermissionsPolicy
            headers["Referrer-Policy"] = "no-referrer"
            headers["Cache-Control"] = "no-store"
            headers["X-Content-Type-Options"] = "nosniff"
        }
        guard
            let httpResponse = HTTPURLResponse(
                url: url, statusCode: status, httpVersion: "HTTP/1.1", headerFields: headers)
        else {
            task.didFailWithError(URLError(.badServerResponse))
            return
        }
        task.didReceive(httpResponse)
        task.didReceive(data)
        task.didFinish()
    }

    private static func mimeType(for ext: String) -> String {
        switch ext.lowercased() {
        case "html": return "text/html; charset=utf-8"
        case "js", "mjs": return "text/javascript"
        case "css": return "text/css"
        case "woff2": return "font/woff2"
        case "woff": return "font/woff"
        // KaTeX ships woff2/woff/ttf per face; iOS always wins on woff2, but a
        // served .ttf/.otf still deserves a real font MIME, not octet-stream.
        case "ttf": return "font/ttf"
        case "otf": return "font/otf"
        case "svg": return "image/svg+xml"
        case "png": return "image/png"
        case "jpg", "jpeg": return "image/jpeg"
        case "json": return "application/json"
        default: return "application/octet-stream"
        }
    }
}
