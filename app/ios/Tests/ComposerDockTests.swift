import SwiftUI
import Testing
import UIKit

@testable import Baybo

/// The dock chain, RASTERIZED. Everything else about the attach panel is tested
/// through geometry (`AttachMenuTests`) or through the accessibility tree
/// (`ComposerAttachUITests`), and `50b4e33f` slipped past both: a `.clipped()`
/// here erased the panel's PAINT while its layout, its hit region and its
/// accessibility frame all stayed correct. `box()` was right, `isHittable` was
/// right, and the user saw a `+` that dimmed the screen and showed nothing.
/// Pixels are the only witness, and this is the cheap tier to read them on —
/// no simulator UI automation, ~milliseconds.
///
/// It renders the REAL `ComposerDock` with the REAL `AttachMenuPanel` inside it;
/// only the composer's own rows are stood in for (they need a `ChatStore`). A
/// stand-in reproducing the chain instead would be worse than no test: it would
/// stay green the next time `ChatScreen` diverged from it.
@Suite @MainActor
struct ComposerDockTests {
    /// The `+`'s frame in dock space on a bare dock — the panel hangs off its x.
    private static let plus = CGRect(x: 40, y: 0, width: 46, height: 48)
    private static let dockWidth: CGFloat = 402
    private static let dockHeight: CGFloat = 60
    /// Room ABOVE the dock for the panel to float into. The panel's box is
    /// entirely negative (`y ∈ [-102, -10]`), and `ImageRenderer` sizes its
    /// image to the view it is handed — so without headroom the panel would fall
    /// outside the bitmap whether or not anything clipped it, and the test would
    /// pass for the wrong reason.
    private static let headroom: CGFloat = 160

    /// The dock as the screen builds it, in a container that leaves the panel
    /// somewhere to be drawn. The composer's rows are a plain block: what is
    /// under test is the chain, not the pill.
    private static func harness(panelOpen: Bool, collapsed: Bool = false) -> some View {
        VStack(spacing: 0) {
            Color.clear.frame(width: dockWidth, height: headroom)
            ComposerDock(collapsed: collapsed, jumpVisible: false) {
                Theme.surface.frame(width: dockWidth, height: dockHeight)
            } panel: {
                if panelOpen {
                    AttachMenuPanel(anchor: plus, isPresented: .constant(true)) { _ in }
                }
            }
        }
        .frame(width: dockWidth, height: headroom + dockHeight)
        .background(Theme.paper)
    }

    /// Ink pixels inside `rect`, in the rendered image's own coordinate space
    /// (points × scale). The panel's rows are a glyph and a label in `Theme.ink`
    /// (0x111111) over glass; the band is otherwise flat paper. Counting DARK
    /// pixels rather than diffing images keeps the assertion independent of how
    /// the glass material happens to rasterize, which is the one part of this
    /// view an offscreen renderer may not reproduce faithfully.
    private static func inkPixels(_ image: UIImage, in rect: CGRect) -> Int {
        guard let cg = image.cgImage else { return 0 }
        let scale = CGFloat(cg.width) / dockWidth
        let box = CGRect(
            x: rect.minX * scale, y: rect.minY * scale,
            width: rect.width * scale, height: rect.height * scale
        ).intersection(CGRect(x: 0, y: 0, width: cg.width, height: cg.height))
        guard !box.isNull, box.width >= 1, box.height >= 1,
            let crop = cg.cropping(to: box)
        else { return 0 }

        let width = crop.width
        let height = crop.height
        var bytes = [UInt8](repeating: 0, count: width * height * 4)
        guard
            let ctx = CGContext(
                data: &bytes, width: width, height: height, bitsPerComponent: 8,
                bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return 0 }
        ctx.draw(crop, in: CGRect(x: 0, y: 0, width: width, height: height))

        var dark = 0
        for idx in stride(from: 0, to: bytes.count, by: 4) {
            let luma =
                0.299 * Double(bytes[idx]) + 0.587 * Double(bytes[idx + 1])
                + 0.114 * Double(bytes[idx + 2])
            if luma < 128 { dark += 1 }
        }
        return dark
    }

    /// Where the panel lands in the HARNESS's space: `box()` is in dock space,
    /// whose origin is `headroom` points down.
    private static var panelRect: CGRect {
        AttachMenuPanel.box(anchor: plus).offsetBy(dx: 0, dy: headroom)
    }

    private static func render(_ view: some View) -> UIImage {
        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        guard let image = renderer.uiImage else {
            Issue.record("ImageRenderer produced no image")
            return UIImage()
        }
        return image
    }

    /// THE regression. An open panel must put ink where `box()` says it is.
    @Test func theOpenPanelPaintsAboveTheDock() {
        let ink = Self.inkPixels(Self.render(Self.harness(panelOpen: true)), in: Self.panelRect)
        #expect(ink > 100, "the attach panel drew nothing in its own box (\(ink) ink pixels)")
    }

    /// …and the measurement is not vacuous: the same band is empty with the
    /// panel down, so the count above is the panel and not the harness.
    @Test func theSameBandIsEmptyWithThePanelDown() {
        let ink = Self.inkPixels(Self.render(Self.harness(panelOpen: false)), in: Self.panelRect)
        #expect(ink == 0, "the closed dock painted \(ink) ink pixels above itself")
    }

    /// The collapse an expanded HTML preview asks for hides the dock — panel
    /// included — WITHOUT clipping it. `opacity` is what does the hiding; a clip
    /// would take the panel with it in every other state too.
    @Test func aCollapsedDockShowsNothing() {
        let ink = Self.inkPixels(
            Self.render(Self.harness(panelOpen: true, collapsed: true)), in: Self.panelRect)
        #expect(ink == 0, "a collapsed dock still painted \(ink) ink pixels")
    }
}
