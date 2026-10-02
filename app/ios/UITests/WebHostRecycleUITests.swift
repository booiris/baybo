import XCTest

/// Headless drive of the resume recycle (`AppStore.recycleWebHostsIfStale`):
/// a conversation on screen when the app went to the background must still be
/// PAINTED after a long-enough suspension replaces the transcript webview under
/// it. `-baybo-web-recycle-after` shrinks the five-minute threshold to seconds.
///
/// The failure this guards is a rebuilt host that the on-screen `ChatScreen`
/// never adopts — the screen keeps the torn-down view, which has no superview
/// and paints nothing. Accessibility alone cannot see that (a laid-out but
/// unpainted page can still expose its text), so the check is on pixels: ink
/// in the thread's area of the screenshot.
final class WebHostRecycleUITests: BayboUITestCase {
    private static let recycleAfterSeconds = 2

    func testAConversationOnScreenRepaintsInPlaceAfterARecycle() throws {
        let app = launch([
            "-baybo-open-chat", "-baybo-demo-index",
            "-baybo-web-recycle-after", "\(Self.recycleAfterSeconds)",
        ])
        dismissNotificationPrompt()
        XCTAssertTrue(
            demoParagraph(app).waitForExistence(timeout: Self.webviewTimeout),
            "the demo thread never rendered")
        XCTAssertTrue(threadHasInk(), "the thread painted nothing before the suspension")

        // Park the reader up in the history: the recycle must bring them back
        // here rather than to the newest edge.
        let webView = app.webViews.firstMatch
        webView.swipeDown(velocity: .slow)
        webView.swipeDown(velocity: .slow)
        sleep(1)
        attachScreenshot(app, name: "parked before suspension")
        let parked = try XCTUnwrap(topParagraph(app), "no paragraph under the header")
        let parkedLabel = parked.label
        let parkedY = parked.frame.minY

        XCUIDevice.shared.press(.home)
        sleep(UInt32(Self.recycleAfterSeconds + 2))
        app.activate()
        dismissNotificationPrompt()

        XCTAssertTrue(
            demoParagraph(app).waitForExistence(timeout: Self.webviewTimeout),
            "the recycled transcript never re-rendered the conversation")
        let deadline = Date().addingTimeInterval(Self.webviewTimeout)
        var painted = threadHasInk()
        while !painted, Date() < deadline {
            usleep(500_000)
            painted = threadHasInk()
        }
        attachScreenshot(app, name: "after recycle")
        XCTAssertTrue(painted, "the recycled transcript is on screen but painted nothing")

        let restored = app.webViews.staticTexts
            .matching(NSPredicate(format: "label == %@", parkedLabel)).firstMatch
        XCTAssertTrue(restored.waitForExistence(timeout: 5))
        let settle = Date().addingTimeInterval(5)
        while abs(restored.frame.minY - parkedY) > Self.positionTolerance, Date() < settle {
            usleep(200_000)
        }
        XCTAssertEqual(
            restored.frame.minY, parkedY, accuracy: Self.positionTolerance,
            "the reader was not put back where they were parked")
    }

    private static let positionTolerance: CGFloat = 24
    /// Long enough to name one paragraph, not a glyph like the work marker.
    private static let minAnchorLength = 12
    /// Below the native header veil.
    private static let headerClearance: CGFloat = 120

    /// The first paragraph the reader sees under the header.
    private func topParagraph(_ app: XCUIApplication) -> XCUIElement? {
        app.webViews.staticTexts.allElementsBoundByIndex.first {
            $0.frame.minY >= Self.headerClearance && $0.label.count >= Self.minAnchorLength
        }
    }

    private func demoParagraph(_ app: XCUIApplication) -> XCUIElement {
        app.webViews.staticTexts
            .matching(NSPredicate(format: "label MATCHES %@", ".{\(Self.minAnchorLength),}"))
            .firstMatch
    }

    /// The first-launch notification prompt holds the scene at `.inactive`,
    /// and `didBecomeActive` — where the recycle runs — never fires under it.
    private func dismissNotificationPrompt() {
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let alert = springboard.alerts.firstMatch
        guard alert.waitForExistence(timeout: 5) else { return }
        let allow = alert.buttons["Allow"]
        if allow.exists {
            allow.tap()
        } else {
            alert.buttons.element(boundBy: alert.buttons.count - 1).tap()
        }
        _ = alert.waitForNonExistence(timeout: 3)
    }

    /// Dark samples across the band between the header and the composer, where
    /// only transcript text can put them.
    private func threadHasInk() -> Bool {
        guard let pixels = screenPixels() else { return false }
        let band = pixels.size.height * 0.3...pixels.size.height * 0.7
        var dark = 0
        for y in stride(from: band.lowerBound, to: band.upperBound, by: 8) {
            for x in stride(from: 16, to: pixels.size.width - 16, by: 8) {
                if let luma = pixels.brightness(at: CGPoint(x: x, y: y)), luma < 0.35 {
                    dark += 1
                }
            }
        }
        return dark > 20
    }
}
