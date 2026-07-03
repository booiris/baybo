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
        // The bundle is served over a custom scheme, NOT file:// — Vite's
        // module scripts fail CORS from a file origin (see the handler's doc).
        config.setURLSchemeHandler(
            TranscriptSchemeHandler(), forURLScheme: TranscriptSchemeHandler.scheme)
        let webView = WKWebView(frame: .zero, configuration: config)
        webView.isOpaque = false
        webView.backgroundColor = .white
        webView.scrollView.backgroundColor = .white
        // The bundle lays out its own insets (`--thread-top-inset` under the
        // native header veil); automatic adjustment would double them.
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        // Dragging down over the thread lowers the keyboard (chat convention;
        // the old web app's overflow scroll behaved the same way).
        webView.scrollView.keyboardDismissMode = .interactive
        #if DEBUG
        webView.isInspectable = true
        #endif
        bridge.webView = webView

        if let url = TranscriptSchemeHandler.indexURL {
            webView.load(URLRequest(url: url))
        }
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {}

    static func dismantleUIView(_ webView: WKWebView, coordinator: ()) {
        webView.configuration.userContentController
            .removeScriptMessageHandler(forName: TranscriptBridge.messageHandlerName)
    }
}
