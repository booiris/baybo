import SwiftUI
import WebKit

/// One of the issue surface's two kept-warm rendering engines.
///
/// The hosts belong to `IssueHostPool`, not to a card. A visit keeps its own
/// `IssueStore`; the pool retargets one of these already-loaded pages to that
/// store before navigation starts. Two are the minimum for a native push: the
/// source and destination are both visible during the transition.
@MainActor
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
