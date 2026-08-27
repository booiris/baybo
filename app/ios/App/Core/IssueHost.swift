import SwiftUI
import WebKit

@MainActor
/// Kept-warm renderer owned by the two-slot pool rather than by one card visit.
final class IssueHost {
    static let issueURL = URL(string: "\(TranscriptSchemeHandler.scheme)://localhost/issue.html")

    let bridge: IssueBridge
    let webView: WKWebView
    private let navigationPolicy = IssueNavigationPolicy()

    private var tornDown = false

    init() {
        let bridge = IssueBridge()
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

        if let url = Self.issueURL {
            webView.load(URLRequest(url: url))
        }
    }

    func retarget(to store: IssueStore, targetId: String) {
        bridge.retarget(to: store, targetId: targetId)
    }

    func clearTarget(_ targetId: String) {
        bridge.clearTarget(targetId)
    }

    func teardown() {
        guard !tornDown else { return }
        tornDown = true
        bridge.teardown()
        webView.stopLoading()
        webView.configuration.userContentController
            .removeScriptMessageHandler(forName: IssueBridge.messageHandlerName)
        webView.removeFromSuperview()
    }

    deinit {
        MainActor.assumeIsolated {
            teardown()
        }
    }
}

@MainActor
/// Recovers visible WebKit crashes and prevents agent-authored links from
/// navigating the privileged main frame; external links open through native.
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
