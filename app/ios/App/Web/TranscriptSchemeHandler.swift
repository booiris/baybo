import Foundation
import WebKit

/// Serves the bundled transcript (`App/Resources/transcript/`) over a custom
/// scheme. `file://` cannot host it: Vite emits `<script type="module"
/// crossorigin>`, and WebKit fetches module scripts with CORS — a request from
/// a file (opaque) origin is rejected, so the bundle never executes and the
/// bridge never comes up. A scheme handler answers with real HTTP semantics
/// (correct MIME + permissive CORS), which module loading accepts.
final class TranscriptSchemeHandler: NSObject, WKURLSchemeHandler {
    static let scheme = "baybo-transcript"
    static let indexURL = URL(string: "\(scheme)://localhost/index.html")

    func webView(_ webView: WKWebView, start urlSchemeTask: WKURLSchemeTask) {
        guard let url = urlSchemeTask.request.url,
            let root = Bundle.main.url(forResource: "transcript", withExtension: nil)
        else {
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

    func webView(_ webView: WKWebView, stop urlSchemeTask: WKURLSchemeTask) {}

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
