import QuickLook
import SwiftUI
import UIKit

/// Presents a tapped file attachment. SwiftUI has no `quickLookPreview` on iOS
/// (it is macOS-only), so this wraps `QLPreviewController` — and falls back to
/// the share sheet for the types QuickLook cannot render (archives, arbitrary
/// binaries), where "save to Files" is the only useful action anyway.
struct FilePreviewSheet: View {
    let url: URL

    var body: some View {
        if QLPreviewController.canPreview(url as QLPreviewItem) {
            QuickLookView(url: url).ignoresSafeArea()
        } else {
            ShareSheet(url: url)
        }
    }
}

private struct QuickLookView: UIViewControllerRepresentable {
    let url: URL

    func makeUIViewController(context: Context) -> QLPreviewController {
        let controller = QLPreviewController()
        controller.dataSource = context.coordinator
        return controller
    }

    func updateUIViewController(_: QLPreviewController, context _: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(url: url) }

    /// `QLPreviewController` keeps only an unowned reference to its data source,
    /// so it has to outlive the controller — the coordinator is that lifetime.
    final class Coordinator: NSObject, QLPreviewControllerDataSource {
        private let url: URL

        init(url: URL) { self.url = url }

        func numberOfPreviewItems(in _: QLPreviewController) -> Int { 1 }

        func previewController(_: QLPreviewController, previewItemAt _: Int) -> QLPreviewItem {
            url as NSURL
        }
    }
}

/// The system share sheet over a file on disk. Shared with the image viewer's
/// share button — passing the FILE (not a re-encoded `UIImage`) is what puts the
/// original bytes and real filename into Save-to-Files / AirDrop / other apps.
struct ShareSheet: UIViewControllerRepresentable {
    let url: URL

    func makeUIViewController(context _: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [url], applicationActivities: nil)
    }

    func updateUIViewController(_: UIActivityViewController, context _: Context) {}
}
