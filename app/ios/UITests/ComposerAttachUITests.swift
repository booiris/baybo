import XCTest

/// Headless drive of the composer's attachment surface: the `+` panel and the
/// staged strip.
///
/// Staging FOR REAL is not reachable from a UI test — the system document
/// picker and the photo picker both run out of process — so the strip is
/// seeded by `-baybo-demo-compose` (one of each state) and the panel is driven
/// by hand. Between them they cover the two halves the picker sits between.
final class ComposerAttachUITests: BayboUITestCase {
    private static let stagedFile = "staged-architecture-review-2026-Q3.pdf"
    private static let uploadingFile = "staged-landing-flow-capture.mp4"
    private static let failedFile = "staged-quarterly.numbers"
    private static let stagedImage = "Image"
    private static let plusLabel = "Add attachment"
    private static let photosRow = "Photos"
    private static let filesRow = "Files"
    private static let jumpToLatestLabel = "Jump to the latest message"
    /// The transcript's own attachments, driven to `ready`, so the chat has
    /// something to present full-screen over the composer.
    private static let transcriptArguments = [
        "-baybo-open-chat", "-baybo-demo-compose", "-baybo-demo-attachments",
        "-baybo-demo-download",
    ]

    /// The `+` is the only attachment entry point, and its panel is what
    /// replaced the bare photo picker — a regression here is a composer that
    /// can only attach images again.
    ///
    /// The panel is HAND-ROLLED (`AttachMenuPanel`, overlaid on the dock, its
    /// scrim over the transcript), not a system `Menu`, and the two assertions
    /// past the rows are that difference: a `UIMenu` dims the whole screen and
    /// takes the composer out of reach, where this one leaves the `+` itself
    /// live under it, and its own scrim is what dismisses it.
    ///
    /// This is the BARE dock — the one shape in which the old anchoring also
    /// worked, which is why the two cases below carry the shapes that broke.
    func testPlusPanelOffersPhotosAndFiles() {
        let app = launch(["-baybo-open-chat"])
        let photos = openPanel(app)
        XCTAssertTrue(
            app.buttons[Self.plusLabel].isHittable, "the dock must stay out of the panel's scrim")
        attachScreenshot(app, name: "composer-plus-panel")

        // The panel must PAINT, not merely lay out. Everything else in this
        // file — exists / isHittable / frame intersections — is satisfied just
        // as well by a panel that renders nothing, and that is exactly what
        // shipped: 50b4e33f put a `.clipped()` on the dock's modifier chain
        // AFTER the overlay hosting the panel, and the panel is drawn entirely
        // at negative y (`AttachMenuPanel.box`), so the clip erased its paint
        // while leaving it laid out, hit-testable and green across this suite.
        //
        // A before/after screenshot DIFF is not the test: the scrim dims that
        // whole band on open whether or not the panel drew, and a diff passes
        // on the dimming alone (measured — it does). What only a real panel
        // produces is STRUCTURE: a glyph and a label over glass. This launch
        // has no thread behind the panel, so the band is otherwise one flat
        // colour.
        let shades = Self.distinctShades(
            app.screenshot().image, in: photos.frame, window: app.windows.firstMatch.frame)
        XCTAssertGreaterThan(
            shades, 8,
            "the + panel must RENDER where the accessibility tree says it is — "
                + "the Photos row's band holds \(shades) distinct shades, i.e. it is blank")

        // Anywhere over the transcript is the scrim; the panel itself sits just
        // above the dock, far below this.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.3)).tap()
        XCTAssertTrue(
            photos.waitForNonExistence(timeout: 3), "an outside tap must close the panel")
    }

    /// The dock GROWS UPWARD — a notice line, an approval card, the staged
    /// strip — and the `+` does not move up with it. A panel hung off the `+`
    /// therefore opens INSIDE the dock, and `.safeAreaInset` composites the
    /// dock above the screen's stack: with the four tiles this flag seeds, 68
    /// of the panel's 92pt drew behind the strip and the Files row was
    /// completely hidden. Every row assertion here is `isHittable`, never
    /// `exists`: an occluded row is laid out and found by name exactly like a
    /// visible one, which is how a green suite shipped this.
    func testPlusPanelClearsTheStagedStrip() {
        let app = launch(["-baybo-open-chat", "-baybo-demo-compose"])
        let staged = tile(app, Self.stagedImage)
        XCTAssertTrue(staged.waitForExistence(timeout: 5), "the seeded strip must render")

        let photos = openPanel(app)
        XCTAssertTrue(photos.isHittable, "the Photos row must be reachable over the staged strip")
        let files = app.buttons[Self.filesRow]
        XCTAssertTrue(files.isHittable, "the Files row must be reachable over the staged strip")

        // Clearing the dock's top edge, not merely drawing over it: the strip
        // the user is adding to stays visible and live under an open panel.
        XCTAssertFalse(
            photos.frame.intersects(staged.frame), "the panel must not cover the staged strip")
        XCTAssertFalse(files.frame.intersects(staged.frame), "nor may its bottom row")
        attachScreenshot(app, name: "composer-plus-panel-staged")
    }

    /// The jump-to-latest disc shares the composer's `.safeAreaInset` stack —
    /// 12pt above the dock, horizontally CENTRED, 44×44 — which puts it inside
    /// the panel's band and ABOVE a panel drawn in the screen's stack. 44pt of
    /// the Files row scrolled the transcript instead of picking a file.
    ///
    /// The disc is raised by DRAGGING, not by `-baybo-demo-jump`. That flag
    /// fires a flat `scrollBy(-1400)` exactly 4s after launch and takes the disc
    /// back 3s later, which needs a thread already that tall at 4s — and
    /// `launch()` forces `-baybo-reset-store`, so the demo thread is still
    /// streaming in from nothing at that moment and the scroll moves zero
    /// pixels. The disc never appeared and this case failed on its own
    /// precondition twice, saying nothing about the panel. Dragging waits for
    /// the content first and leaves the disc up indefinitely.
    func testPlusPanelClearsTheJumpButton() {
        let app = launch(["-baybo-open-chat", "-baybo-demo-frames"])
        // The send button reverts from Stop to Send when the demo turn ends —
        // the one signal from outside the webview that the thread is now as
        // tall as it is going to get.
        XCTAssertTrue(
            app.buttons["Send"].waitForExistence(timeout: Self.webviewTimeout),
            "the demo thread must finish streaming before there is anything to scroll")

        let jump = app.buttons[Self.jumpToLatestLabel]
        for _ in 0..<4 where !jump.exists {
            app.swipeDown()
        }
        XCTAssertTrue(
            jump.waitForExistence(timeout: 5),
            "dragging the transcript off its newest edge must raise the jump disc")

        let photos = openPanel(app)
        let jumpFrame = jump.frame
        XCTAssertTrue(jump.exists, "the disc must still be up while the panel is measured")
        XCTAssertTrue(photos.isHittable, "the Photos row must be reachable beside the jump disc")
        let files = app.buttons[Self.filesRow]
        XCTAssertTrue(files.isHittable, "the Files row must be reachable beside the jump disc")
        XCTAssertFalse(
            photos.frame.intersects(jumpFrame), "the panel must clear the jump disc, not share it")
        XCTAssertFalse(files.frame.intersects(jumpFrame), "nor may its bottom row")
        attachScreenshot(app, name: "composer-plus-panel-jump")
    }

    /// Open the `+` panel and hand back its first row.
    private func openPanel(_ app: XCUIApplication) -> XCUIElement {
        let plus = app.buttons[Self.plusLabel]
        XCTAssertTrue(plus.waitForExistence(timeout: 5), "the composer's + must exist")
        plus.tap()
        let photos = app.buttons[Self.photosRow]
        XCTAssertTrue(photos.waitForExistence(timeout: 3), "the + panel must offer Photos")
        XCTAssertTrue(app.buttons[Self.filesRow].exists, "the + panel must offer Files")
        return photos
    }

    /// Every staged state renders its own tile: a ready file shows its size, a
    /// mid-upload one counts bytes, a failed one offers the retry.
    func testStagedStripRendersEveryState() {
        let app = launch(["-baybo-open-chat", "-baybo-demo-compose"])

        let ready = tile(app, Self.stagedFile)
        XCTAssertTrue(ready.waitForExistence(timeout: 5), "a staged file must render a tile")
        XCTAssertTrue(tile(app, "Image").exists, "a staged photo must render a tile")

        // The byte counter beside the spinner IS the upload's progress, the
        // same way a download presents it.
        let uploading = tile(app, Self.uploadingFile)
        XCTAssertTrue(uploading.exists)
        XCTAssertTrue(
            (uploading.value as? String)?.contains("/") == true,
            "an uploading tile must count its bytes against the total")
        XCTAssertEqual(
            tile(app, Self.failedFile).value as? String, "Retry",
            "a failed tile must offer the retry")
        attachScreenshot(app, name: "composer-staged-strip")
    }

    /// The trap this test exists for: `ChatScreen` docks the composer in a
    /// `.safeAreaInset`, and a `fullScreenCover` over the chat (the video
    /// player here, the image viewer identically) tears that content down and
    /// puts it straight back — the same reason chat audio stops on
    /// `AppStore.chatPath` and NOT on `ChatScreen.onDisappear`. Reclaiming the
    /// strip on the composer's own teardown therefore cancelled every staged
    /// upload and emptied the strip because the user tapped a video in the
    /// transcript, and the send that followed shipped the message MINUS its
    /// attachments, silently.
    ///
    /// A `.sheet` (QuickLook here) leaves the presenter on screen and fires no
    /// teardown at all — the control for the same assertion, and the reason the
    /// two are driven in one launch.
    func testStagedStripSurvivesEveryPresentationOverTheChat() {
        let app = launch(Self.transcriptArguments)
        let staged = tile(app, Self.failedFile)
        XCTAssertTrue(
            staged.waitForExistence(timeout: Self.webviewTimeout), "the seeded strip must render")
        waitForDemoDownload(app)

        // The cover: the native video player over the chat.
        app.buttons[Self.videoReadyLabel].tap()
        let share = app.buttons["viewer.square.and.arrow.up"]
        XCTAssertTrue(share.waitForExistence(timeout: 5), "the player must present")
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        let close = app.buttons["Close Button"]
        XCTAssertTrue(close.waitForExistence(timeout: 3), "AVKit must supply its close button")
        close.tap()
        XCTAssertTrue(share.waitForNonExistence(timeout: 5), "close must dismiss the player")

        XCTAssertTrue(
            staged.waitForExistence(timeout: 5),
            "a fullScreenCover over the chat is not the user leaving it — the strip must survive")
        XCTAssertEqual(
            staged.value as? String, "Retry",
            "and every tile keeps the state it was in")
        attachScreenshot(app, name: "composer-staged-after-cover")

        // The sheet: QuickLook on the transcript's PDF card. Its label is the
        // card's name + meta line; nothing in the strip shares it.
        app.buttons.matching(NSPredicate(format: "label CONTAINS 'final.pdf'")).firstMatch.tap()
        let previewClose = app.buttons["viewer.xmark"]
        XCTAssertTrue(previewClose.waitForExistence(timeout: 5), "the preview must present")
        previewClose.tap()
        XCTAssertTrue(previewClose.waitForNonExistence(timeout: 5))

        XCTAssertTrue(staged.waitForExistence(timeout: 5), "nor is a sheet over it")
    }

    /// How many distinct pixel values a window-space `rect` holds. The only way
    /// to tell a painted panel from a clipped-away one: the accessibility tree
    /// reports the two identically, and a scrim dims the band either way.
    private static func distinctShades(_ image: UIImage, in rect: CGRect, window: CGRect) -> Int {
        guard !rect.isEmpty, !window.isEmpty, let full = image.cgImage else { return 0 }
        let scale = CGFloat(full.width) / window.width
        let box = CGRect(
            x: rect.minX * scale, y: rect.minY * scale,
            width: rect.width * scale, height: rect.height * scale)
        guard let crop = full.cropping(to: box),
            let provider = crop.dataProvider, let data = provider.data
        else { return 0 }
        let bytes = Data(referencing: data)
        let stride = max(1, crop.bitsPerPixel / 8)
        var seen = Set<UInt32>()
        for offset in Swift.stride(from: 0, to: bytes.count - stride, by: stride) {
            var pixel: UInt32 = 0
            for byte in 0..<Swift.min(4, stride) {
                pixel = (pixel << 8) | UInt32(bytes[bytes.startIndex + offset + byte])
            }
            seen.insert(pixel)
            if seen.count > 64 { return seen.count }
        }
        return seen.count
    }

    /// A staged tile is one accessibility element with the name as its label
    /// and the size/progress line as its value; the element TYPE SwiftUI picks
    /// for a combined container is not contractual, so match on the label.
    private func tile(_ app: XCUIApplication, _ label: String) -> XCUIElement {
        app.descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", label))
            .firstMatch
    }
}
