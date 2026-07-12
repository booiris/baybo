import XCTest

/// Headless drive of the attachment long-press → system share sheet path
/// (web gesture → `shareFile` bridge msg → `ChatStore.fileShare` →
/// `ShareSheet`). Runs against the `-baybo-demo-attachments -baybo-demo-download`
/// turn: the download drive walks the PDF card to `ready` and
/// `demoMaterializeBytes` serves stand-in bytes, so the sheet genuinely
/// presents with no gateway. The transcript is a webview, but its cards
/// surface as accessibility Buttons (the file card's label is its
/// name + meta line), so the press targets the element itself.
final class AttachmentShareUITests: BayboUITestCase {
    private static let demoArguments = [
        "-baybo-open-chat", "-baybo-demo-attachments", "-baybo-demo-download",
    ]

    private func pdfCard(_ app: XCUIApplication) -> XCUIElement {
        app.buttons.matching(NSPredicate(format: "label CONTAINS 'final.pdf'")).firstMatch
    }

    /// Launch, wait out the download drive, then long-press the ready PDF card.
    /// Idle and ready render the same meta line, so the card's own existence
    /// cannot prove readiness — the video tile's label flip can, and one drive
    /// walks both cards.
    private func launchAndPressPdfCard() -> XCUIApplication {
        let app = launch(Self.demoArguments)
        let card = pdfCard(app)
        XCTAssertTrue(card.waitForExistence(timeout: 10), "demo PDF card must render")
        waitForDemoDownload(app)
        card.press(forDuration: 0.7)
        return app
    }

    func testLongPressOnReadyFileCardPresentsShareSheet() {
        let app = launchAndPressPdfCard()

        // The system sheet's stable container identifier — the filename header
        // lives in a remote process and is not reliably addressable.
        XCTAssertTrue(
            app.otherElements["ActivityListView"].waitForExistence(timeout: 5),
            "long-press on a ready file card must raise the share sheet")
    }

    /// A long-press that fired must NOT also run the card's tap action — the
    /// synthetic click after the lift is swallowed web-side. If suppression
    /// broke, QuickLook would present over (or instead of) the share sheet.
    func testLongPressDoesNotAlsoOpenPreview() {
        let app = launchAndPressPdfCard()
        XCTAssertTrue(app.otherElements["ActivityListView"].waitForExistence(timeout: 5))

        // QuickLook's chrome (its Done button) must not be anywhere.
        XCTAssertFalse(
            app.buttons["QLOverlayDoneButtonAccessibilityIdentifier"].exists,
            "the click after a fired long-press must be suppressed")
    }

    /// The VIDEO player relies on AVKit's OWN close button (a second overlaid
    /// ✕ read as a duplicate) — so its dismissal must genuinely clear the
    /// fullScreenCover binding, or the tile could never present twice. The
    /// demo video materialises as a real 2s clip, so the genuine player chrome
    /// renders. Our share disc marks the player being on screen.
    func testVideoPlayerAVKitCloseDismissesAndReopens() {
        let app = launch(Self.demoArguments)

        // The tile's ready label IS the drive's completion signal.
        waitForDemoDownload(app)
        let tile = app.buttons[Self.videoReadyLabel]
        tile.tap()

        let share = app.buttons["viewer.square.and.arrow.up"]
        XCTAssertTrue(share.waitForExistence(timeout: 5), "player must present")

        // AVKit auto-hides its chrome; a tap on the surface summons it.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        // Match AVKit's own IDENTIFIER, not a label: the label is localized
        // ("Close" / "关闭"), and an EMBEDDED AVPlayerViewController shows a
        // "Close Button" where the fullscreen presentation shows "Done" — so a
        // label match is wrong twice over, and passed here only by matching the
        // Chinese label on a Chinese simulator.
        let close = app.buttons["Close Button"]
        guard close.waitForExistence(timeout: 3) else {
            XCTFail("AVKit must supply its close button")
            return
        }
        close.tap()
        XCTAssertTrue(share.waitForNonExistence(timeout: 5), "close must dismiss the player")

        // The binding must have cleared, or a second tap presents nothing.
        tile.tap()
        XCTAssertTrue(share.waitForExistence(timeout: 5), "tile must present the player again")
    }

    /// The file PREVIEW's overlaid chrome: a plain tap opens QuickLook, whose
    /// embedded controller has no chrome of its own — the viewer discs supply
    /// share (top-right) and close (top-left). Drives share → sheet → dismiss
    /// → ✕ → preview gone.
    func testPreviewChromeSharesAndCloses() {
        let app = launch(Self.demoArguments)
        let card = pdfCard(app)
        XCTAssertTrue(card.waitForExistence(timeout: 10))
        waitForDemoDownload(app)
        card.tap()

        let close = app.buttons["viewer.xmark"]
        let share = app.buttons["viewer.square.and.arrow.up"]
        XCTAssertTrue(close.waitForExistence(timeout: 5), "preview must carry the ✕ disc")
        XCTAssertTrue(share.exists, "preview must carry the share disc")

        share.tap()
        let sheet = app.otherElements["ActivityListView"]
        XCTAssertTrue(sheet.waitForExistence(timeout: 5), "share disc must raise the sheet")
        app.buttons["header.closeButton"].tap()
        XCTAssertTrue(sheet.waitForNonExistence(timeout: 5))

        close.tap()
        XCTAssertTrue(
            close.waitForNonExistence(timeout: 5),
            "✕ must dismiss the preview")
    }
}
