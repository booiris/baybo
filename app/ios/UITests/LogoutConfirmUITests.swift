import XCTest

/// The regression the hand-rolled `ConfirmDialog` exists to prevent: with the
/// stock `.confirmationDialog`, a scrim-tap dismissal left `isPresented`
/// latched true, so the next logout tap wrote true-over-true and nothing
/// presented. Drives the real hit-test path: present → scrim-dismiss →
/// re-present → cancel-dismiss → re-present.
final class LogoutConfirmUITests: XCTestCase {
    func testConfirmReopensAfterScrimAndCancelDismissals() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-baybo-open-home", "-baybo-home-tab", "settings", "-baybo.lang", "en",
        ]
        app.launch()

        let logoutPill = app.buttons["Log out"]
        XCTAssertTrue(logoutPill.waitForExistence(timeout: 5))
        logoutPill.tap()

        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 3), "confirm did not present")

        // Scrim tap: the top strip is always outside the centered card. Let the
        // entrance grace period lapse first — inside it scrim taps are ignored.
        Thread.sleep(forTimeInterval: 0.6)
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.08)).tap()
        XCTAssertTrue(
            cancel.waitForNonExistence(timeout: 3), "scrim tap did not cancel")

        logoutPill.tap()
        XCTAssertTrue(
            cancel.waitForExistence(timeout: 3),
            "confirm did not re-present after a scrim dismiss")

        cancel.tap()
        XCTAssertTrue(
            cancel.waitForNonExistence(timeout: 3), "Cancel did not dismiss")

        logoutPill.tap()
        XCTAssertTrue(
            cancel.waitForExistence(timeout: 3),
            "confirm did not re-present after a Cancel dismiss")
    }

    /// A fast second tap lands where the trigger was — which is scrim once the
    /// dialog starts presenting. Without an entrance grace period it cancels a
    /// dialog the user never saw: haptic fires, nothing appears, the button
    /// reads as dead.
    func testRapidDoubleTapStillPresentsConfirm() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-baybo-open-home", "-baybo-home-tab", "settings", "-baybo.lang", "en",
        ]
        app.launch()

        let logoutPill = app.buttons["Log out"]
        XCTAssertTrue(logoutPill.waitForExistence(timeout: 5))
        logoutPill.doubleTap()

        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 2), "confirm not up after a double tap")
        // The dialog must also still be there once the entrance fully settles.
        Thread.sleep(forTimeInterval: 1.0)
        XCTAssertTrue(cancel.exists, "confirm was cancelled by the double tap's second touch")
    }
}
