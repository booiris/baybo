import SwiftUI
import WebKit

@MainActor
final class TranscriptHost {
    let bridge: TranscriptBridge
    let webView: WKWebView
    private let navigationPolicy = TranscriptNavigationPolicy()

    init(store: any TranscriptTarget) {
        let bridge = TranscriptBridge(store: store)
        self.bridge = bridge

        let config = WKWebViewConfiguration()
        config.userContentController.add(bridge, name: TranscriptBridge.messageHandlerName)
        // Vite module scripts fail CORS from a file origin.
        config.setURLSchemeHandler(
            TranscriptSchemeHandler(dynamicRoute: .htmlPreview),
            forURLScheme: TranscriptSchemeHandler.scheme)

        let webView = WKWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = navigationPolicy
        webView.isOpaque = false
        webView.backgroundColor = .white
        webView.scrollView.backgroundColor = .white
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        webView.scrollView.keyboardDismissMode = .interactive
        #if DEBUG
            webView.isInspectable = true
        #endif
        bridge.webView = webView
        self.webView = webView

        if let url = TranscriptSchemeHandler.indexURL {
            webView.load(URLRequest(url: url))
        }
    }

    func teardown() {
        webView.stopLoading()
        webView.configuration.userContentController
            .removeScriptMessageHandler(forName: TranscriptBridge.messageHandlerName)
        webView.removeFromSuperview()
    }
}

@MainActor
private final class TranscriptNavigationPolicy: NSObject, WKNavigationDelegate {
    func webView(
        _ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction
    ) async -> WKNavigationActionPolicy {
        guard let url = navigationAction.request.url,
            let targetFrame = navigationAction.targetFrame,
            TranscriptSchemeHandler.permitsNavigation(
                to: url, isMainFrame: targetFrame.isMainFrame)
        else {
            return .cancel
        }
        return .allow
    }
}
