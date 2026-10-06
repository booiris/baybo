import WebKit

@MainActor
protocol BottomInsetSink: AnyObject {
    func pushBottomInset()
}

/// Composer geometry and page readiness can both land before the webview is
/// in a window (a notification open), and neither re-fires after attachment.
/// Retrying on attachment/layout lets the sink replay its retained composer
/// edge; the sink dedups on whole pixels.
@MainActor
final class InsetTrackingWebView: WKWebView {
    weak var insetSink: (any BottomInsetSink)?

    override func didMoveToWindow() {
        super.didMoveToWindow()
        insetSink?.pushBottomInset()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        insetSink?.pushBottomInset()
    }
}
