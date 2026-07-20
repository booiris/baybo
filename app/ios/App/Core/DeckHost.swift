import SwiftUI
import WebKit

/// The app's SECOND webview (the first is the transcript's): the deck shell,
/// prewarmed once a binding reaches home, kept warm, torn down on logout/rebind.
/// The shell rides the same bundled dist + scheme handler as the transcript —
/// `deck.html` is just another entry in `App/Resources/transcript/`.
@MainActor
final class DeckHost {
    static let deckURL = URL(string: "\(TranscriptSchemeHandler.scheme)://localhost/deck.html")

    let bridge: DeckBridge
    let webView: WKWebView

    init(store: DeckStore) {
        let bridge = DeckBridge()
        bridge.store = store
        self.bridge = bridge

        let config = WKWebViewConfiguration()
        config.userContentController.add(bridge, name: DeckBridge.messageHandlerName)
        config.setURLSchemeHandler(
            TranscriptSchemeHandler(blobRouteEnabled: true),
            forURLScheme: TranscriptSchemeHandler.scheme)

        let webView = WKWebView(frame: .zero, configuration: config)
        webView.isOpaque = false
        webView.backgroundColor = .white
        webView.scrollView.backgroundColor = .white
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        #if DEBUG
            webView.isInspectable = true
        #endif
        bridge.webView = webView
        self.webView = webView
        store.bridge = bridge

        if let url = Self.deckURL {
            webView.load(URLRequest(url: url))
        }
    }

    func teardown() {
        webView.stopLoading()
        webView.configuration.userContentController
            .removeScriptMessageHandler(forName: DeckBridge.messageHandlerName)
        webView.removeFromSuperview()
    }
}

/// Reparenting shim (the `TranscriptWebView` idiom): `makeUIView` returns the
/// host's webview; dismantle only unparents.
struct DeckWebView: UIViewRepresentable {
    let host: DeckHost

    func makeUIView(context: Context) -> WKWebView {
        host.webView
    }

    func updateUIView(_ uiView: WKWebView, context: Context) {}

    static func dismantleUIView(_ uiView: WKWebView, coordinator: ()) {
        uiView.removeFromSuperview()
    }
}
