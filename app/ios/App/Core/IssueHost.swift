import SwiftUI
import WebKit

/// The app's THIRD webview: one project card.
///
/// Unlike the transcript's (one, reused, long-lived) and the deck's (one,
/// prewarmed, kept warm), this one is per-card and dies with its screen. A card
/// is entered, read, acted on and left; keeping a warm one would mean keeping
/// its card's state warm too, and the next card is a different card. The cost
/// is a cold WebContent process per open, which is what the page's own loading
/// line is for.
///
/// **Dying with the screen is what `deinit` is for**, and it is why nothing
/// here hangs off an appearance callback. The destructor used to run from
/// `ProjectIssueScreen`'s `.onDisappear`, which SwiftUI ALSO fires when a push
/// merely covers the card: tapping a sub-issue or the `↳ #N` parent chip
/// destroyed the card underneath, and coming back found a webview nothing
/// re-parents — a white page under a live header and dock. Ownership is the
/// only signal that tells "covered" from "gone".
///
/// The owner is the card's `IssueStore`, which is the screen's one
/// `@StateObject` (`IssueStore.host`) — so the chain is
/// screen → store → host → webview, and the pop takes all four. `SubagentHost`
/// and `ProjectRunHost` are a second `@StateObject` beside their store instead,
/// because their screens OBSERVE published state on them and a nested
/// `ObservableObject` republishes nothing through its owner. Nothing here is
/// observed — the screen only calls through `bridge` — so the extra object
/// would buy nothing and cost the store an eager construction on every SwiftUI
/// update.
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
        // The one-shot frame, sent from the one-shot object. `IssueBridge`
        // buffers into `pending` until the page answers `ready`, so it does not
        // matter that this runs before the load has finished — and sending it
        // here is what lets the screen stop guarding a rebuild it must never do.
        bridge.deliverInit(language: Lang.shared.current.lproj, bottomInset: 0)
    }

    /// The card is gone: stop the page, unhook the native surface, and let the
    /// WebContent process go.
    ///
    /// The store is not detached here and does not need to be — it is this
    /// object's OWNER, so it is already on its way out, its
    /// `ProjectInvalidations` token unregisters itself, and `TranscriptMedia`
    /// holds its sink weakly.
    deinit {
        MainActor.assumeIsolated {
            webView.stopLoading()
            webView.configuration.userContentController
                .removeScriptMessageHandler(forName: IssueBridge.messageHandlerName)
            webView.removeFromSuperview()
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

/// Reparenting shim — `TranscriptWebView`'s, verbatim, which is what the doc
/// here always claimed and the body did not: unparenting belongs at the moment
/// the view is CLAIMED, never at the moment it is dropped.
struct IssueWebView: UIViewRepresentable {
    let host: IssueHost

    func makeUIView(context: Context) -> WKWebView {
        host.webView.removeFromSuperview()
        return host.webView
    }

    func updateUIView(_ uiView: WKWebView, context: Context) {}

    static func dismantleUIView(_ uiView: WKWebView, coordinator: ()) {
        // Host-owned. Removing it here can race the next screen's reparenting.
    }
}
