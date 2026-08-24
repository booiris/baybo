import SwiftUI
import WebKit

/// The app's THIRD webview: one project card.
///
/// Unlike the transcript's (one, reused, long-lived) and the deck's (one,
/// prewarmed, kept warm), this one is per-card and torn down on exit. A card
/// is entered, read, acted on and left; keeping a warm one would mean keeping
/// its card's state warm too, and the next card is a different card. The cost
/// is a cold WebContent process per open, which is what the page's own loading
/// line is for.
@MainActor
final class IssueHost {
    static let issueURL = URL(string: "\(TranscriptSchemeHandler.scheme)://localhost/issue.html")

    let bridge: IssueBridge
    let webView: WKWebView
    private let navigationPolicy = IssueNavigationPolicy()

    init(store: IssueStore) {
        let bridge = IssueBridge()
        bridge.store = store
        self.bridge = bridge

        let config = WKWebViewConfiguration()
        config.userContentController.add(bridge, name: IssueBridge.messageHandlerName)
        config.setURLSchemeHandler(
            TranscriptSchemeHandler(dynamicRoute: .staticOnly),
            forURLScheme: TranscriptSchemeHandler.scheme)

        let webView = WKWebView(frame: .zero, configuration: config)
        navigationPolicy.bridge = bridge
        webView.navigationDelegate = navigationPolicy
        webView.isOpaque = false
        webView.backgroundColor = .white
        webView.scrollView.backgroundColor = .white
        // The page owns its own scrolling and its own bottom inset; letting
        // UIKit add a safe-area inset on top would double the dock clearance.
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        #if DEBUG
            webView.isInspectable = true
        #endif
        bridge.webView = webView
        self.webView = webView
        store.attach(bridge)

        if let url = Self.issueURL {
            webView.load(URLRequest(url: url))
        }
    }

    func teardown(store: IssueStore) {
        store.detach(bridge)
        webView.stopLoading()
        webView.configuration.userContentController
            .removeScriptMessageHandler(forName: IssueBridge.messageHandlerName)
        webView.removeFromSuperview()
    }
}

/// Two jobs. A visible-time WebContent death is the host's to recover (WebKit
/// auto-reloads only offscreen views), and the page may navigate its own main
/// frame to exactly one place: itself.
///
/// That second one is not paranoia about our own bundle — the card body renders
/// **agent-authored markdown**, and a link in a description that navigated the
/// main frame would replace the card with whatever it pointed at, inside a
/// webview holding the native message handler. Links go to the system browser
/// through `openUrl` instead.
@MainActor
private final class IssueNavigationPolicy: NSObject, WKNavigationDelegate {
    weak var bridge: IssueBridge?

    func webView(
        _ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
    ) {
        guard navigationAction.targetFrame?.isMainFrame == true else {
            decisionHandler(.allow)
            return
        }
        let url = navigationAction.request.url
        decisionHandler(url == IssueHost.issueURL ? .allow : .cancel)
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        bridge?.contentProcessDied()
    }
}

/// Reparenting shim (the `TranscriptWebView` idiom): `makeUIView` returns the
/// host's webview; dismantle only unparents.
struct IssueWebView: UIViewRepresentable {
    let host: IssueHost

    func makeUIView(context: Context) -> WKWebView {
        host.webView
    }

    func updateUIView(_ uiView: WKWebView, context: Context) {}

    static func dismantleUIView(_ uiView: WKWebView, coordinator: ()) {
        uiView.removeFromSuperview()
    }
}
