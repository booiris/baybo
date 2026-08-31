import XCTest

/// The two things about a transcript image that only exist above the seam: the
/// box it is laid out in, and what happens when a finger lands on it.
///
/// Both are about VECTORS, because both broke for vectors only. An SVG carries
/// no pixels, so it has no natural size to read off the element showing it and
/// no bitmap for `UIImage` to decode — and the transcript did both of those
/// things. What it recorded as a wide diagram's size was the loading tile it
/// happened to decode inside, so the NEXT open of the thread reserved that box
/// and the diagram came back a third of the width it first rendered at; and a
/// tap on one fell out of `viewImage`'s guard and did nothing at all.
///
/// `-baybo-demo-images` carries a raster spread plus both spellings of a wide
/// SVG (declared `width`/`height`, and a bare `viewBox`), served locally — so
/// this needs no gateway.
final class AttachmentImageUITests: BayboUITestCase {
    private static let demoArguments = ["-baybo-open-chat", "-baybo-demo-images"]
    /// Cards in the demo turn, in `DemoFrames.demoImageSizes` order.
    private static let bannerIndex = 1
    private static let declaredVectorIndex = 4
    private static let viewBoxVectorIndex = 5
    private static let cardCount = 6
    private static let loadingLabel = "Loading image…"

    /// The transcript is a webview, but each decoded image card surfaces as a
    /// Button — labelled, not identified, since the label is what the web side
    /// owns (`chat.viewImage`, pinned to English by `launch`).
    private func cards(_ app: XCUIApplication) -> XCUIElementQuery {
        app.buttons.matching(NSPredicate(format: "label == %@", "View image"))
    }

    /// Every card of the demo turn, decoded. The button appears when its blob
    /// URL lands, before the image's `onLoad` retires the loading frame. Waiting
    /// for both prevents a tap from landing while the decoded cards reflow.
    private func waitForCards(_ app: XCUIApplication) -> [XCUIElement] {
        let all = cards(app)
        let loading = app.descendants(matching: .any).matching(
            NSPredicate(format: "label == %@", Self.loadingLabel))
        let deadline = Date().addingTimeInterval(Self.webviewTimeout)
        repeat {
            if all.count >= Self.cardCount && loading.count == 0 { break }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        } while Date() < deadline
        XCTAssertEqual(
            all.count, Self.cardCount,
            "the demo turn's image cards never all decoded")
        XCTAssertEqual(
            loading.count, 0,
            "the demo turn's image cards never finished decoding")
        return all.allElementsBoundByIndex
    }

    /// A wide vector takes the whole reading column — the same width the wide
    /// RASTER banner beside it takes, which is what "as wide as the thread"
    /// means here without hard-coding a device width.
    func testAWideVectorFillsTheReadingColumn() {
        let app = launch(Self.demoArguments)
        let widths = waitForCards(app).map(\.frame.width)

        XCTAssertEqual(
            widths[Self.declaredVectorIndex], widths[Self.bannerIndex], accuracy: 1,
            "an SVG declaring 1200x400 must fill the column like the 1600x400 banner does")
        XCTAssertEqual(
            widths[Self.viewBoxVectorIndex], widths[Self.bannerIndex], accuracy: 1,
            "an SVG with only a viewBox has no intrinsic width — with no box reserved "
                + "for it, it lays out at zero and never appears at all")
    }

    /// The reported bug, in the order it happens: right on the first paint,
    /// wrong on every open after it. The second launch keeps the store and drops
    /// the demo feeder, so the thread comes back from its MIRROR — the cold path
    /// the sizes were being poisoned for.
    func testAVectorKeepsItsWidthAcrossARestart() {
        let first = launch(Self.demoArguments)
        let live = waitForCards(first).map(\.frame.width)
        first.terminate()

        let restored = waitForCards(reopenFromMirror()).map(\.frame.width)

        XCTAssertEqual(
            restored[Self.declaredVectorIndex], live[Self.declaredVectorIndex], accuracy: 1,
            "the reopened thread reserved a different box for the SVG than it first "
                + "rendered at — the size recorded was its loading tile's, not its own")
        XCTAssertEqual(
            restored[Self.viewBoxVectorIndex], live[Self.viewBoxVectorIndex], accuracy: 1)
    }

    /// A tap lands on the element's CENTRE, and the demo turn is taller than the
    /// screen — the thread opens at its newest edge, so the earliest cards can
    /// sit under the header. XCUITest still calls a partly clipped web element
    /// hittable, so use the actual header and dock as the interactive viewport.
    private func tap(_ card: XCUIElement, in app: XCUIApplication) {
        let visibleTop = app.buttons["Back to conversations"].frame.maxY
        let visibleBottom = app.buttons["composer-attach"].frame.minY
        for _ in 0..<6 {
            let frame = card.frame
            if card.isHittable && frame.minY >= visibleTop && frame.maxY <= visibleBottom {
                break
            }
            if frame.midY < (visibleTop + visibleBottom) / 2 {
                app.swipeDown()
            } else {
                app.swipeUp()
            }
        }
        XCTAssertTrue(
            card.isHittable && card.frame.minY >= visibleTop && card.frame.maxY <= visibleBottom,
            "the card never cleared the header and dock to be tapped")
        card.tap()
    }

    /// Tapping an SVG has to open the viewer. It opened nothing: `viewImage`
    /// decoded with `UIImage(data:)`, which is nil for an SVG on every iOS
    /// there is, and the tap died in the guard with no viewer and no error.
    func testTappingAVectorOpensTheViewer() {
        let app = launch(Self.demoArguments)
        tap(waitForCards(app)[Self.declaredVectorIndex], in: app)

        let close = app.buttons["viewer.xmark"]
        XCTAssertTrue(
            close.waitForExistence(timeout: 10),
            "tapping an SVG opened no image viewer")
        attachScreenshot(app, name: "vector-image-viewer")

        close.tap()
        XCTAssertTrue(
            close.waitForNonExistence(timeout: 5), "the viewer would not close")
    }

    /// Double-tap zooms the open vector, and a second one puts it back — the
    /// same two-state gesture the raster viewer binds by hand.
    ///
    /// It has to be bound by hand HERE too, which is the bug this pins: WebKit's
    /// own double tap is smart magnification ("zoom to the block under the
    /// finger"), and this page is a single image already fitted to the viewport,
    /// so WebKit computes that there is nothing to do and the gesture does
    /// nothing at all.
    ///
    /// Measured in PIXELS, because the zoom happens inside a web view whose page
    /// scale nothing above the seam can read: the demo diagram is light on the
    /// viewer's black field, so how much of a vertical line through the middle
    /// of the screen is bright IS how tall the art is drawn.
    func testDoubleTapZoomsAVectorAndBack() throws {
        let app = launch(Self.demoArguments)
        tap(waitForCards(app)[Self.declaredVectorIndex], in: app)
        XCTAssertTrue(
            app.buttons["viewer.xmark"].waitForExistence(timeout: 10), "the viewer never opened")

        let middle = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        let fitted = try artHeight("at fit")
        middle.doubleTap()
        let zoomed = try artHeight("after a double tap", settle: 1.2)
        attachScreenshot(app, name: "vector-viewer-zoomed")
        middle.doubleTap()
        let restored = try artHeight("after a second double tap", settle: 1.2)

        XCTAssertGreaterThan(
            zoomed, fitted * 2,
            "a double tap left the diagram the size it already was — nothing zoomed")
        XCTAssertEqual(
            restored, fitted, accuracy: 8,
            "the second double tap did not restore the fit")
    }

    /// How tall the diagram is drawn, in points, off the screen's own pixels.
    private func artHeight(_ what: String, settle: TimeInterval = 0.4) throws -> CGFloat {
        // The zoom animates; there is no observable to wait on above the seam.
        RunLoop.current.run(until: Date().addingTimeInterval(settle))
        let pixels = try XCTUnwrap(screenPixels(), "could not read the screen \(what)")
        return try XCTUnwrap(
            pixels.brightHeight(atX: pixels.size.width / 2),
            "could not measure the diagram \(what)")
    }

    /// The raster path, unchanged — here so a regression in the shared election
    /// can't hide behind the vector cases.
    func testTappingARasterImageStillOpensTheViewer() {
        let app = launch(Self.demoArguments)
        tap(waitForCards(app)[Self.bannerIndex], in: app)

        XCTAssertTrue(
            app.buttons["viewer.xmark"].waitForExistence(timeout: 10),
            "tapping a PNG opened no image viewer")
    }

    /// The same conversation, opened cold: the store survives (no
    /// `-baybo-reset-store`) and the demo feeder is gone, so every row comes
    /// from the transcript mirror rather than being pushed again.
    private func reopenFromMirror() -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-baybo-open-chat", "-baybo.lang", "en", "-AppleLanguages", "(en)",
            "-AppleLocale", "en_US",
        ]
        app.launch()
        reopened = app
        return app
    }

    private var reopened: XCUIApplication?

    override func tearDown() {
        reopened?.terminate()
        reopened = nil
        super.tearDown()
    }
}
