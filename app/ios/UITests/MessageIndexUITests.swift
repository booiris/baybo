import XCTest

/// The chat header's message index (`MessageIndexSheet`), driven through the
/// real hit-test path: the trailing glass circle appears once the transcript
/// has posted an outline, its VALUE carries the count (the label is the
/// constant "Message history", the model pill's split), and picking a row
/// dismisses the sheet so the jump can run on a clear screen.
///
/// Everything below the seam — the grouping, the labels, the jump message — is
/// covered far more cheaply by the unit tiers. What is only reachable here is
/// the presentation lifecycle: a sheet that does not close on a row tap leaves
/// the user parked on a panel over the message they asked to see.
final class MessageIndexUITests: BayboUITestCase {
    private static let chatArguments = [
        "-baybo-open-chat", "-baybo-demo-index",
    ]
    /// `MessageIndexSheet.rowIdentifier` — the app's symbols are not visible to
    /// the UI target, and every row's label is transcript prose.
    private static let rowIdentifier = "message-index-row"
    private static let buttonLabel = "Message history"
    private static let jumpToLatestLabel = "Jump to the latest message"
    /// The demo fixture sends at least this many messages.
    private static let fixtureSends = 5

    func testPickingAMessageDismissesTheSheet() throws {
        let app = launch(Self.chatArguments)

        let indexButton = app.buttons[Self.buttonLabel]
        XCTAssertTrue(
            indexButton.waitForExistence(timeout: Self.webviewTimeout),
            "the message-index button never appeared — the transcript posted no outline")
        XCTAssertTrue(
            waitUntilCount(indexButton, atLeast: Self.fixtureSends),
            "the button's count value never reached the fixture's sends "
                + "(value: \(String(describing: indexButton.value)))")

        indexButton.tap()

        let title = app.staticTexts[Self.buttonLabel]
        XCTAssertTrue(title.waitForExistence(timeout: 5), "the sheet did not present")
        let rows = app.buttons.matching(identifier: Self.rowIdentifier)
        XCTAssertTrue(
            rows.firstMatch.waitForExistence(timeout: 5), "the sheet rendered no message rows")
        attachScreenshot(app, name: "message-index")

        // The topmost visible row. The sheet opens on the reader's position and
        // the fixture leaves that at the newest edge, so this is always an
        // EARLIER message — i.e. a jump that has to travel.
        let row = try XCTUnwrap(
            rows.allElementsBoundByIndex.first(where: \.isHittable), "no message row is hittable")
        row.tap()

        XCTAssertTrue(
            title.waitForNonExistence(timeout: 5),
            "picking a message left the sheet on screen")

        // The whole round trip in one signal: the tap crossed the bridge, the
        // web side scrolled off the newest edge, and its `jumpVisible` mirror
        // raised the native jump-to-latest circle — which is also the user's
        // way back. Without the scroll this button never appears.
        XCTAssertTrue(
            app.buttons[Self.jumpToLatestLabel].waitForExistence(timeout: 5),
            "the jump never moved the transcript off the newest edge")
        attachScreenshot(app, name: "message-index-landing")
    }

    /// The count rides the element VALUE and reads `24+` over a windowed set,
    /// so parse the leading digits; `XCUIElement.value` has no waitFor — poll.
    private func waitUntilCount(
        _ element: XCUIElement, atLeast expected: Int, timeout: TimeInterval = 10
    ) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        repeat {
            if let count = Self.count(of: element), count >= expected { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        } while Date() < deadline
        return false
    }

    private static func count(of element: XCUIElement) -> Int? {
        guard let value = element.value as? String else { return nil }
        return Int(value.prefix { $0.isNumber })
    }
}
