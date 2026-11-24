import SwiftUI
import WebKit

struct TranscriptWebView: UIViewRepresentable {
    let webView: WKWebView

    func makeUIView(context: Context) -> UIView {
        let viewport = TranscriptViewport()
        viewport.addSubview(webView)
        return viewport
    }

    func updateUIView(_ viewport: UIView, context: Context) {}
}

private final class TranscriptViewport: UIView {
    override func layoutSubviews() {
        super.layoutSubviews()
        // SwiftUI measures a newly inserted representable at a temporary size.
        // Keep the shared document at its previous size until the new host lays out.
        guard !bounds.isEmpty, let webView = subviews.first, webView.frame != bounds else { return }
        webView.frame = bounds
    }
}
