import XCTest

/// Headless drive of the attachment long-press → system share sheet path
/// (web gesture → `shareFile` bridge msg → `ChatStore.fileShare` →
/// `ShareSheet`). Runs against the `-baybo-demo-attachments -baybo-demo-download`
/// turn: the download drive walks the PDF card to `ready` (~8s in) and
/// `demoMaterializeBytes` serves stand-in bytes, so the sheet genuinely
/// presents with no gateway. The transcript is a webview, but its cards
/// surface as accessibility Buttons (the file card's label is its
/// name + meta line), so the press targets the element itself.
final class AttachmentShareUITests: XCTestCase {
    private func launchAndPressPdfCard(_ app: XCUIApplication) {
        app.launchArguments = [
            "-baybo-open-chat", "-baybo-demo-attachments", "-baybo-demo-download",
        ]
        app.launch()

        let card = app.buttons.matching(
            NSPredicate(format: "label CONTAINS 'final.pdf'")
        ).firstMatch
        XCTAssertTrue(card.waitForExistence(timeout: 10), "demo PDF card must render")
        // Idle and ready share the same meta line, so existence can't prove
        // ready — wait out the demo download drive (ready at ~7.2s).
        sleep(10)
        card.press(forDuration: 0.7)
    }

    func testLongPressOnReadyFileCardPresentsShareSheet() {
        let app = XCUIApplication()
        launchAndPressPdfCard(app)

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
        let app = XCUIApplication()
        launchAndPressPdfCard(app)
        XCTAssertTrue(app.otherElements["ActivityListView"].waitForExistence(timeout: 5))

        // QuickLook's chrome (its Done button) must not be anywhere.
        XCTAssertFalse(
            app.buttons["QLOverlayDoneButtonAccessibilityIdentifier"].exists,
            "the click after a fired long-press must be suppressed")
    }
}
