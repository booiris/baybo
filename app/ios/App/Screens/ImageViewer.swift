import SwiftUI
import UIKit

/// A chat image the user tapped, decoded and awaiting the full-screen viewer.
/// Carries the `UIImage` so the viewer needs no second fetch, plus the same bytes
/// materialised on disk under their real name — the share sheet hands over the
/// FILE, so Save-to-Photos / Files / AirDrop get the original encoding and name
/// rather than a re-encoded bitmap. Identified by blob id so
/// `.fullScreenCover(item:)` re-presents when a different image is tapped.
struct ViewedImage: Identifiable {
    let id: String
    let image: UIImage
    let url: URL?
}

/// Full-screen viewer for a chat image: pinch to zoom, double-tap to toggle zoom
/// and restore to fit, single-tap or the ✕ to close, on a black field the image
/// fades onto. A dedicated viewer rather than QuickLook — QuickLook embedded in a
/// SwiftUI sheet gave no reliable double-tap-to-restore (the sheet's own gestures
/// fight it), and this matches the edge-to-edge chat-image feel the document
/// previewer's white chrome does not.
struct ImageViewer: View {
    let image: UIImage
    /// The image on disk under its real name — the share sheet's item. Absent
    /// only if writing it failed, in which case the share button is hidden rather
    /// than offering a dead action.
    let url: URL?
    let onClose: () -> Void
    @State private var shown = false
    @State private var sharing = false

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            ZoomableImageView(image: image, onSingleTap: onClose)
                .ignoresSafeArea()
                .opacity(shown ? 1 : 0)
            VStack {
                HStack {
                    circleButton("xmark", action: onClose)
                    Spacer()
                    if url != nil {
                        circleButton("square.and.arrow.up") { sharing = true }
                    }
                }
                .padding(.horizontal, 16)
                .padding(.top, 8)
                Spacer()
            }
            .opacity(shown ? 1 : 0)
        }
        .statusBarHidden(true)
        // Fade the image onto the black field rather than snapping it in.
        .onAppear { withAnimation(.easeOut(duration: 0.22)) { shown = true } }
        .sheet(isPresented: $sharing) {
            if let url {
                ShareSheet(url: url)
            }
        }
    }

    /// The viewer's chrome: a glass disc over the image, legible on any picture.
    private func circleButton(_ symbol: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 34, height: 34)
                .background(.ultraThinMaterial, in: Circle())
        }
    }
}

/// UIScrollView-backed zoomable image — the reliable way to get pinch, momentum,
/// centering, and a double-tap that returns to fit. SwiftUI's `MagnificationGesture`
/// can't center or reset the zoom cleanly.
private struct ZoomableImageView: UIViewRepresentable {
    let image: UIImage
    let onSingleTap: () -> Void

    func makeUIView(context: Context) -> LayoutReportingScrollView {
        let scroll = LayoutReportingScrollView()
        scroll.delegate = context.coordinator
        scroll.bouncesZoom = true
        scroll.showsVerticalScrollIndicator = false
        scroll.showsHorizontalScrollIndicator = false
        scroll.backgroundColor = .clear
        scroll.contentInsetAdjustmentBehavior = .never

        let imageView = UIImageView(image: image)
        imageView.isUserInteractionEnabled = true
        scroll.addSubview(imageView)
        context.coordinator.scrollView = scroll
        context.coordinator.imageView = imageView

        // The fit can only be computed once UIKit has handed the scroll view its
        // real bounds. SwiftUI's `updateUIView` runs BEFORE that (bounds are still
        // zero), so a fit driven from there is skipped and never recomputed —
        // leaving zoomScale/min/max all at 1, i.e. the image at native size with
        // no zoom and nothing for double-tap to restore to. Drive it from the
        // scroll view's own layout pass instead.
        let coordinator = context.coordinator
        scroll.onLayout = { [weak coordinator] in coordinator?.layout() }

        let doubleTap = UITapGestureRecognizer(
            target: coordinator, action: #selector(Coordinator.handleDoubleTap(_:)))
        doubleTap.numberOfTapsRequired = 2
        scroll.addGestureRecognizer(doubleTap)

        // Single-tap closes; it waits for the double-tap to fail so a zoom gesture
        // isn't read as a dismiss.
        let singleTap = UITapGestureRecognizer(
            target: coordinator, action: #selector(Coordinator.handleSingleTap))
        singleTap.numberOfTapsRequired = 1
        singleTap.require(toFail: doubleTap)
        scroll.addGestureRecognizer(singleTap)

        return scroll
    }

    func updateUIView(_: LayoutReportingScrollView, context _: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(onSingleTap: onSingleTap) }

    /// Reports every layout pass, so the fit is computed the moment the scroll
    /// view actually has bounds (and again if they change, e.g. on rotation).
    final class LayoutReportingScrollView: UIScrollView {
        var onLayout: (() -> Void)?

        override func layoutSubviews() {
            super.layoutSubviews()
            onLayout?()
        }
    }

    final class Coordinator: NSObject, UIScrollViewDelegate {
        weak var scrollView: UIScrollView?
        weak var imageView: UIImageView?
        private let onSingleTap: () -> Void
        /// The bounds the fit was last computed for. Guards the setup so it runs
        /// only on a real frame change — re-seating the image's frame on the
        /// layout passes that zooming itself triggers would fight the scroll view.
        private var fittedFor: CGSize = .zero

        init(onSingleTap: @escaping () -> Void) { self.onSingleTap = onSingleTap }

        func viewForZooming(in _: UIScrollView) -> UIView? { imageView }
        func scrollViewDidZoom(_: UIScrollView) { center() }

        func layout() {
            guard let scroll = scrollView, let imageView, let img = imageView.image,
                scroll.bounds.width > 0, scroll.bounds.height > 0,
                img.size.width > 0, img.size.height > 0
            else { return }
            if scroll.bounds.size != fittedFor {
                fittedFor = scroll.bounds.size
                // Drop any live zoom transform before re-seating the frame, then
                // fit the image to the bounds and make THAT the minimum zoom, so
                // double-tap has a scale to restore down to.
                scroll.minimumZoomScale = 1
                scroll.maximumZoomScale = 1
                scroll.zoomScale = 1
                imageView.frame = CGRect(origin: .zero, size: img.size)
                scroll.contentSize = img.size
                let fit = min(
                    scroll.bounds.width / img.size.width,
                    scroll.bounds.height / img.size.height)
                scroll.minimumZoomScale = fit
                scroll.maximumZoomScale = fit * 4
                scroll.zoomScale = fit
            }
            center()
        }

        /// Pin the image to the middle when it's smaller than the scroll bounds
        /// (at fit, or a portrait image in a landscape frame).
        private func center() {
            guard let scroll = scrollView, let imageView else { return }
            let x = max(0, (scroll.bounds.width - imageView.frame.width) / 2)
            let y = max(0, (scroll.bounds.height - imageView.frame.height) / 2)
            let inset = UIEdgeInsets(top: y, left: x, bottom: y, right: x)
            if scroll.contentInset != inset { scroll.contentInset = inset }
        }

        @objc func handleSingleTap() { onSingleTap() }

        @objc func handleDoubleTap(_ gr: UITapGestureRecognizer) {
            guard let scroll = scrollView else { return }
            if scroll.zoomScale > scroll.minimumZoomScale {
                // Zoomed in → restore to fit.
                scroll.setZoomScale(scroll.minimumZoomScale, animated: true)
            } else {
                // At fit → zoom toward the tapped point.
                let target = min(scroll.maximumZoomScale, scroll.minimumZoomScale * 3)
                let point = gr.location(in: imageView)
                let w = scroll.bounds.width / target
                let h = scroll.bounds.height / target
                scroll.zoom(
                    to: CGRect(x: point.x - w / 2, y: point.y - h / 2, width: w, height: h),
                    animated: true)
            }
        }
    }
}
