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
    /// The one path a preview's `script-src` admits (see the response policy
    /// below). Everything under it is a blob id, so a page can only run a script
    /// somebody already put in the blob store and handed it the read token for.
    private static let libraryPathPrefix = "/html-lib/"
    private static let deckBlobServeCap = 8 * 1024 * 1024
    private static let htmlPreviewServeCap = 16 * 1024 * 1024
    private static let libraryServeCap = 8 * 1024 * 1024
    private static let htmlPreviewCSP =
        "default-src 'none'; script-src 'unsafe-inline' baybo-transcript://localhost/html-lib/; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none';"
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
        /// agent-authored page — inline style, no network, no images beyond
        /// `data:`. It draws its chart as inline SVG and loads nothing: a demo
        /// session has no leg, so a `/html-lib/` blob could only ever fail, and
        /// a fixture whose permanent state is an error teaches nobody anything.
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
              h1{margin:0 0 2px;font-size:17px}p.sub{margin:0 0 14px;color:#9a9aa2;font-size:12px}
              svg{width:100%;height:auto;display:block}
              .grid{stroke:#2a2d36;stroke-width:1}
              .axis{fill:#9a9aa2;font:10px ui-monospace,SFMono-Regular,monospace}
              .row{display:flex;gap:12px;margin-top:20px}
              .kpi{flex:1;border:1px solid #2a2d36;border-radius:10px;padding:10px 12px}
              .kpi b{display:block;font-size:20px}.kpi em{font-style:normal;font-size:11px;color:#9a9aa2}
            </style></head><body>
            <h1>Quarterly sales</h1><p class="sub">2026 · thousands · sample data</p>
            <svg viewBox="0 0 320 180" role="img" aria-label="Quarterly sales, rising from 93 to 190">
              <line class="grid" x1="34" y1="150" x2="310" y2="150"/>
              <line class="grid" x1="34" y1="115" x2="310" y2="115"/>
              <line class="grid" x1="34" y1="80" x2="310" y2="80"/>
              <line class="grid" x1="34" y1="45" x2="310" y2="45"/>
              <line class="grid" x1="34" y1="10" x2="310" y2="10"/>
              <text class="axis" x="0" y="153">0</text>
              <text class="axis" x="0" y="83">100</text>
              <text class="axis" x="0" y="13">200</text>
              <path d="M34 85 L126 48 L218 66 L310 17 L310 150 L34 150 Z" fill="#8b5cf6" fill-opacity=".20"/>
              <polyline points="34,85 126,48 218,66 310,17" fill="none" stroke="#8b5cf6" stroke-width="2.5"
                stroke-linecap="round" stroke-linejoin="round"/>
              <circle cx="34" cy="85" r="3" fill="#12141a" stroke="#8b5cf6" stroke-width="2"/>
              <circle cx="126" cy="48" r="3" fill="#12141a" stroke="#8b5cf6" stroke-width="2"/>
              <circle cx="218" cy="66" r="3" fill="#12141a" stroke="#8b5cf6" stroke-width="2"/>
              <circle cx="310" cy="17" r="3" fill="#12141a" stroke="#8b5cf6" stroke-width="2"/>
              <text class="axis" x="34" y="170" text-anchor="middle">Q1</text>
              <text class="axis" x="126" y="170" text-anchor="middle">Q2</text>
              <text class="axis" x="218" y="170" text-anchor="middle">Q3</text>
              <text class="axis" x="310" y="170" text-anchor="middle">Q4</text>
            </svg>
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
        if dynamicRoute == .htmlPreview, url.path.hasPrefix(Self.libraryPathPrefix) {
            startLibrary(url: url, task: urlSchemeTask)
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
        /// A vendored library, fetched as a subresource OF a preview. It gets no
        /// CSP of its own — a policy only governs a document — but it does need
        /// the right MIME, or WebKit refuses to execute it under `nosniff`.
        case library
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

    /// A script a preview loads, addressed by blob id — the same id space, the
    /// same transport and the same cache as the preview document itself.
    ///
    /// The product ships no libraries: whatever the page loads here is something
    /// the agent put in the blob store, so this grants the page nothing it did
    /// not already have (it can write arbitrary inline JavaScript regardless).
    /// A bad or missing id fails the LOAD rather than answering empty, so the
    /// page's `onerror` runs — an empty 200 would leave a `<script>` that
    /// "loaded" and defined nothing.
    private func startLibrary(url: URL, task: WKURLSchemeTask) {
        startBlob(
            url: url, task: task,
            blobId: String(url.path.dropFirst(Self.libraryPathPrefix.count)),
            maxBytes: UInt64(Self.libraryServeCap), response: .library)
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
        // Both fail the LOAD rather than answering a body: deck imagery has a
        // broken-image state of its own, and a library that answered 200 with a
        // failure document would be executed as JavaScript.
        case .deck, .library:
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
        case .library:
            headers["Content-Type"] = "text/javascript"
            headers["X-Content-Type-Options"] = "nosniff"
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
