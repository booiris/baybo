import XCTest

/// The full-screen HTML preview and the left edge it borrows.
///
/// What is only reachable here: the swipe. While a preview covers the
/// conversation, a left-edge drag has to leave the PREVIEW — and the way that
/// is arranged is by taking the interactive pop out of the picture for the
/// duration (`EdgeSwipeOverride`). Get that half wrong and the swipe pops the
/// whole chat back to the list, which no unit tier can see: the web half's
/// tests stop at the bridge, and the native half's gesture plumbing only exists
/// against a live UINavigationController.
///
/// Both assertions matter, and they are different failures. The chat header
/// still being there says the pop did NOT fire. The header being HITTABLE again
/// says the preview really did leave — the whole native chrome is
/// `allowsHitTesting(false)` behind an expanded preview, so a swipe that did
/// nothing leaves a header that exists and cannot be touched.
final class HtmlPreviewUITests: BayboUITestCase {
    private static let chatArguments = ["-baybo-open-chat", "-baybo-demo-html"]
    private static let expandLabel = "Show HTML preview full screen"
    private static let closeLabel = "Close full-screen HTML preview"
    /// `chat.back` in `Localizable.xcstrings`, under the `-baybo.lang en` pin.
    private static let backLabel = "Back to conversations"

    func testEdgeSwipeLeavesTheFullScreenPreviewNotTheChat() throws {
        let app = launch(Self.chatArguments)

        let expand = app.buttons[Self.expandLabel]
        XCTAssertTrue(
            expand.waitForExistence(timeout: Self.webviewTimeout),
            "the transcript never rendered the demo turn's HTML preview card")
        attachScreenshot(app, name: "html-preview-inline")

        expand.tap()

        let close = app.buttons[Self.closeLabel]
        XCTAssertTrue(close.waitForExistence(timeout: 10), "the preview never expanded")
        let back = app.buttons[Self.backLabel]
        XCTAssertFalse(
            back.isHittable,
            "the chat header is still live under a full-screen preview")
        attachScreenshot(app, name: "html-preview-maximized")

        // From the very edge, the way the interactive pop is triggered.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.002, dy: 0.5))
            .press(
                forDuration: 0.05,
                thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.9, dy: 0.5)))

        XCTAssertTrue(
            close.waitForNonExistence(timeout: 10),
            "the edge swipe left the preview full screen")
        XCTAssertTrue(
            expand.waitForExistence(timeout: 10),
            "the preview did not come back to its inline card — the swipe popped the chat")
        XCTAssertTrue(
            back.waitForHittable(timeout: 5),
            "native chrome never came back after the preview closed")
        attachScreenshot(app, name: "html-preview-after-swipe")
    }

    /// The dangerous half of the same gesture. A drag released SHORT has to snap
    /// back to full screen — and above all must not have popped the chat on its
    /// way, which is the failure this whole override exists to prevent and the
    /// one a user hits by accident rather than on purpose.
    ///
    /// Driven at `.slow` with a hold before release so neither half of native's
    /// verdict fires: the travel stays under `dismissFraction`, and holding
    /// bleeds the release velocity to nothing rather than betting on whatever
    /// XCUITest's default drag leaves behind.
    func testShortEdgeSwipeSnapsBackAndKeepsTheChat() throws {
        let app = launch(Self.chatArguments)

        let expand = app.buttons[Self.expandLabel]
        XCTAssertTrue(
            expand.waitForExistence(timeout: Self.webviewTimeout),
            "the transcript never rendered the demo turn's HTML preview card")
        expand.tap()

        let close = app.buttons[Self.closeLabel]
        XCTAssertTrue(close.waitForExistence(timeout: 10), "the preview never expanded")

        app.coordinate(withNormalizedOffset: CGVector(dx: 0.002, dy: 0.5))
            .press(
                forDuration: 0.05,
                thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.15, dy: 0.5)),
                withVelocity: .slow,
                thenHoldForDuration: 0.4)

        XCTAssertTrue(close.exists, "a short edge swipe dismissed the preview anyway")
        XCTAssertFalse(
            app.buttons[Self.backLabel].isHittable,
            "a short edge swipe popped the chat out from under the preview")
        attachScreenshot(app, name: "html-preview-short-swipe")
    }
}

extension XCUIElement {
    /// `waitForExistence` is not enough for the chrome behind an expanded
    /// preview: it never stops existing, it stops being touchable.
    func waitForHittable(timeout: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        repeat {
            if isHittable { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        } while Date() < deadline
        return false
    }
}
