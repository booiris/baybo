import SwiftUI
import WebKit

struct TranscriptWebView: UIViewRepresentable {
    let webView: WKWebView

    func makeUIView(context: Context) -> WKWebView {
        webView.removeFromSuperview()
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {}

    static func dismantleUIView(_ webView: WKWebView, coordinator: ()) {
        // Host-owned. Removing it here can race the next screen's reparenting.
    }
}
