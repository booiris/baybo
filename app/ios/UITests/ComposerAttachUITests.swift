import XCTest

/// Headless drive of the composer's attachment surface: the `+` menu and the
/// staged strip.
///
/// Staging FOR REAL is not reachable from a UI test — the system document
/// picker and the photo picker both run out of process — so the strip is
/// seeded by `-baybo-demo-compose` (one of each state) and the menu is driven
/// by hand. Between them they cover the two halves the picker sits between.
final class ComposerAttachUITests: BayboUITestCase {
    private static let stagedFile = "staged-architecture-review-2026-Q3.pdf"
    private static let uploadingFile = "staged-landing-flow-capture.mp4"
    private static let failedFile = "staged-quarterly.numbers"
    /// The transcript's own attachments, driven to `ready`, so the chat has
    /// something to present full-screen over the composer.
    private static let transcriptArguments = [
        "-baybo-open-chat", "-baybo-demo-compose", "-baybo-demo-attachments",
        "-baybo-demo-download",
    ]

    /// The `+` is the only attachment entry point, and its menu is what
    /// replaced the bare photo picker — a regression here is a composer that
    /// can only attach images again.
    func testPlusMenuOffersPhotosAndFiles() {
        let app = launch(["-baybo-open-chat"])
        let plus = app.buttons["Add attachment"]
        XCTAssertTrue(plus.waitForExistence(timeout: 5), "the composer's + must exist")
        plus.tap()

        XCTAssertTrue(
            app.buttons["Photos"].waitForExistence(timeout: 3),
            "the + menu must offer Photos")
        XCTAssertTrue(app.buttons["Files"].exists, "the + menu must offer Files")
        attachScreenshot(app, name: "composer-plus-menu")
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

    /// A staged tile is one accessibility element with the name as its label
    /// and the size/progress line as its value; the element TYPE SwiftUI picks
    /// for a combined container is not contractual, so match on the label.
    private func tile(_ app: XCUIApplication, _ label: String) -> XCUIElement {
        app.descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", label))
            .firstMatch
    }
}
