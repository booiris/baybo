import SwiftUI
import WebKit

/// The transcript WKWebView: renders the slim web bundle
/// (`App/Resources/transcript/`, built from `web/`) and wires the bridge. The
/// webview owns only the message thread — screens, header, and composer are
/// SwiftUI, so the keyboard never attaches to web content (the entire point of
/// this architecture).
struct TranscriptWebView: UIViewRepresentable {
    @ObservedObject var bridge: TranscriptBridge

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.userContentController.add(bridge, name: TranscriptBridge.messageHandlerName)
        let webView = WKWebView(frame: .zero, configuration: config)
        webView.isOpaque = false
        webView.backgroundColor = .white
        webView.scrollView.backgroundColor = .white
        // The bundle lays out its own insets (`--thread-top-inset` under the
        // native header veil); automatic adjustment would double them.
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        #if DEBUG
        webView.isInspectable = true
        #endif
        bridge.webView = webView

        if let url = Bundle.main.url(
            forResource: "index", withExtension: "html", subdirectory: "transcript")
        {
            webView.loadFileURL(url, allowingReadAccessTo: url.deletingLastPathComponent())
        } else {
            NSLog("baybo: transcript bundle missing from app resources")
        }
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {}

    static func dismantleUIView(_ webView: WKWebView, coordinator: ()) {
        webView.configuration.userContentController
            .removeScriptMessageHandler(forName: TranscriptBridge.messageHandlerName)
    }
}
