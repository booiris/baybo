import SwiftUI
import UIKit
import WebKit

/// A chat image the user tapped, ready for the full-screen viewer. Carries the
/// decoded content so the viewer needs no second fetch, plus the same bytes
/// materialised on disk under their real name — the share sheet hands over the
/// FILE, so Save-to-Photos / Files / AirDrop get the original encoding and name
/// rather than a re-encoded bitmap. Identified by blob id so
/// `.fullScreenCover(item:)` re-presents when a different image is tapped.
struct ViewedImage: Identifiable {
    /// What the viewer has to show — the two are not the same medium.
    ///
    /// A raster blob decodes to a `UIImage` and zooms as pixels. A vector never
    /// decodes at all: iOS has no SVG decoder in any public API (`UIImage(data:)`
    /// answers nil and ImageIO does not carry the type — verified on iOS 26), so
    /// for as long as this asked only `UIImage`, tapping an agent's SVG diagram
    /// was a silent no-op. It keeps its own case rather than being rasterised on
    /// the way in: resolution is the only thing a vector has over a PNG, and a
    /// snapshot taken at one scale is exactly as soft, at 4x zoom, as the tile
    /// the user tapped to get away from.
    enum Content {
        case raster(UIImage)
        case vector(Data)
    }

    let id: String
    let content: Content
    let url: URL?
}

/// The one image mime iOS cannot decode, and the one whose pixels are a
/// rendering decision rather than a property of the bytes.
private let vectorImageMime = "image/svg+xml"

extension ViewedImage.Content {
    /// Raster first: `UIImage` decodes every format the OS knows, and the one it
    /// does not know is the one that must not be rasterised anyway. Nil for
    /// bytes that are neither — a blob that cannot be shown at all opens
    /// nothing, rather than presenting an empty black screen.
    init?(bytes: Data, mimeType: String) {
        if let image = UIImage(data: bytes) {
            self = .raster(image)
            return
        }
        guard mimeType.split(separator: ";").first?.trimmingCharacters(in: .whitespaces)
            .lowercased() == vectorImageMime
        else { return nil }
        self = .vector(bytes)
    }
}

/// Full-screen viewer for a chat image: pinch to zoom, double-tap to toggle zoom
/// and restore to fit, single-tap or the ✕ to close, on a black field the image
/// fades onto. A dedicated viewer rather than QuickLook — QuickLook embedded in a
/// SwiftUI sheet gave no reliable double-tap-to-restore (the sheet's own gestures
/// fight it), and this matches the edge-to-edge chat-image feel the document
/// previewer's white chrome does not.
struct ImageViewer: View {
    let content: ViewedImage.Content
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
            zoomable
                .ignoresSafeArea()
                .opacity(shown ? 1 : 0)
            VStack {
                HStack {
                    ViewerChromeButton(symbol: "xmark", action: onClose)
                    Spacer()
                    if url != nil {
                        ViewerChromeButton(symbol: "square.and.arrow.up") { sharing = true }
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

    /// Both media zoom, centre and close on a single tap; only the engine
    /// underneath differs.
    @ViewBuilder private var zoomable: some View {
        switch content {
        case .raster(let image):
            ZoomableImageView(image: image, onSingleTap: onClose)
        case .vector(let data):
            ZoomableVectorView(data: data, onSingleTap: onClose)
        }
    }
}

/// Full-screen viewer chrome (image viewer, video player, file preview): a
/// glass disc over the content. White glyphs by default — the media viewers
/// sit on black fields; the file preview overrides to `.primary`, because a
/// white glyph vanishes on a white document page.
struct ViewerChromeButton: View {
    let symbol: String
    var tint: Color = .white
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(tint)
                .frame(width: 34, height: 34)
                .background(.ultraThinMaterial, in: Circle())
        }
        .accessibilityIdentifier("viewer.\(symbol)")
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

/// The vector half of the viewer: a WKWebView showing the SVG, fitted to the
/// screen and pinch-zoomable by WebKit's own page zoom.
///
/// A web view rather than the scroll view above because ZOOM is the whole reason
/// a chat image goes full screen, and WebKit re-renders vector art at each scale
/// — a `UIImageView` can only scale whatever raster it was handed, which for an
/// SVG would mean picking a resolution at open time and going soft past it.
///
/// The page is a data-URI `<img>`, never the SVG as the document itself. An SVG
/// document runs its own `<script>`; an SVG inside an `<img>` cannot, by spec.
/// These bytes are agent-authored, so the difference is the whole point — and
/// the CSP (`default-src 'none'`) closes the network besides. The web view is
/// built from a bare configuration with no message handlers, so it shares
/// nothing with the transcript's bridge.
private struct ZoomableVectorView: UIViewRepresentable {
    let data: Data
    let onSingleTap: () -> Void

    /// `initial-scale=1` with the image fitted to the viewport IS the fit state,
    /// so pinch-to-zoom and WebKit's double-tap both have a scale to come back
    /// down to. The black field matches the raster viewer's.
    private static let pageTemplate = #"""
        <!doctype html>
        <html>
        <head>
        <meta charset="utf-8">
        <meta name="viewport" content="width=device-width, initial-scale=1, minimum-scale=1, maximum-scale=10, user-scalable=yes, viewport-fit=cover">
        <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'">
        <style>
          html, body { margin: 0; height: 100%; background: #000; }
          body { display: flex; align-items: center; justify-content: center; }
          img {
            max-width: 100%;
            max-height: 100%;
            -webkit-user-select: none;
            -webkit-touch-callout: none;
          }
        </style>
        </head>
        <body><img src="data:image/svg+xml;base64,{{svg}}" alt=""></body>
        </html>
        """#

    func makeUIView(context: Context) -> WKWebView {
        let web = WKWebView(frame: .zero, configuration: WKWebViewConfiguration())
        web.isOpaque = false
        web.backgroundColor = .black
        web.scrollView.backgroundColor = .black
        web.scrollView.showsVerticalScrollIndicator = false
        web.scrollView.showsHorizontalScrollIndicator = false
        web.scrollView.contentInsetAdjustmentBehavior = .never
        web.allowsLinkPreview = false

        context.coordinator.webView = web

        // The same two taps the raster viewer binds: double to zoom toward the
        // point and back to fit, single to close (waiting for the double to
        // fail so a zoom is never read as a dismiss).
        //
        // The double tap has to be OURS. WebKit's own is smart magnification —
        // "zoom to the block under the finger" — and this page is one image
        // fitted to the viewport, so it computes that there is nothing to do and
        // a double tap does nothing at all. Ours drives the same scroll view its
        // pinch does, so the two agree about where the zoom is.
        let doubleTap = UITapGestureRecognizer(
            target: context.coordinator, action: #selector(Coordinator.handleDoubleTap))
        doubleTap.numberOfTapsRequired = 2
        doubleTap.delegate = context.coordinator
        web.addGestureRecognizer(doubleTap)
        let singleTap = UITapGestureRecognizer(
            target: context.coordinator, action: #selector(Coordinator.handleSingleTap))
        singleTap.numberOfTapsRequired = 1
        singleTap.delegate = context.coordinator
        singleTap.require(toFail: doubleTap)
        web.addGestureRecognizer(singleTap)

        // `baseURL: nil` gives the page a unique opaque origin: no file system,
        // no app-bundle resources, nothing else's storage.
        web.loadHTMLString(
            Self.pageTemplate.replacingOccurrences(
                of: "{{svg}}", with: data.base64EncodedString()),
            baseURL: nil)
        return web
    }

    func updateUIView(_: WKWebView, context _: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(onSingleTap: onSingleTap) }

    final class Coordinator: NSObject, UIGestureRecognizerDelegate {
        /// The zoom is driven through the web view's own scroll view — the same
        /// one its pinch moves — so nothing here has to model the page's scale
        /// separately from WebKit's.
        weak var webView: WKWebView?
        private let onSingleTap: () -> Void
        /// How far a double tap zooms in from fit, matching the raster viewer.
        private static let doubleTapScale: CGFloat = 3

        init(onSingleTap: @escaping () -> Void) { self.onSingleTap = onSingleTap }

        @objc func handleSingleTap() { onSingleTap() }

        @objc func handleDoubleTap(_ gr: UITapGestureRecognizer) {
            guard let scroll = webView?.scrollView else { return }
            if scroll.zoomScale > scroll.minimumZoomScale {
                // Zoomed in → restore to fit.
                scroll.setZoomScale(scroll.minimumZoomScale, animated: true)
                return
            }
            let target = min(
                scroll.maximumZoomScale, scroll.minimumZoomScale * Self.doubleTapScale)
            // `zoom(to:)` wants the rect in the ZOOMING view's space (the page
            // at scale 1), not the scroll view's — the two only agree while the
            // page is unzoomed, which is exactly the branch that never needs it.
            let point = gr.location(in: scroll.delegate?.viewForZooming?(in: scroll) ?? scroll)
            let width = scroll.bounds.width / target
            let height = scroll.bounds.height / target
            scroll.zoom(
                to: CGRect(
                    x: point.x - width / 2, y: point.y - height / 2,
                    width: width, height: height),
                animated: true)
        }

        func gestureRecognizer(
            _: UIGestureRecognizer,
            shouldRecognizeSimultaneouslyWith _: UIGestureRecognizer
        ) -> Bool { true }
    }
}
