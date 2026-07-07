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

    /// Stress the exit-transition window: Cancel, then IMMEDIATELY re-tap the
    /// logout pill (no wait for nonexistence). If the fading scrim still
    /// hit-blocks the pill, the re-tap dies and the dialog never re-presents —
    /// the exact "tapped, nothing happened" the user reports.
    func testImmediateRetapAfterCancelStillPresents() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-baybo-open-home", "-baybo-home-tab", "settings", "-baybo.lang", "en",
        ]
        app.launch()

        let logoutPill = app.buttons["Log out"]
        XCTAssertTrue(logoutPill.waitForExistence(timeout: 5))
        let cancel = app.buttons["Cancel"]

        for round in 1...4 {
            logoutPill.firstMatch.tap()
            XCTAssertTrue(
                cancel.waitForExistence(timeout: 3), "round \(round): confirm did not present")
            cancel.tap()
            // No waitForNonExistence: re-tap while the exit transition runs.
            logoutPill.firstMatch.tap()
            XCTAssertTrue(
                cancel.waitForExistence(timeout: 3),
                "round \(round): immediate re-tap after Cancel was swallowed")
            cancel.tap()
            XCTAssertTrue(cancel.waitForNonExistence(timeout: 3))
        }
    }

    /// The full confirm chain must produce a VISIBLE reaction: tapping the
    /// dialog's destructive button lands on the landing screen (route flips
    /// before any network await — logout is best-effort teardown).
    func testConfirmNavigatesToLanding() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-baybo-open-home", "-baybo-home-tab", "settings", "-baybo.lang", "en",
        ]
        app.launch()

        let logoutPill = app.buttons["Log out"]
        XCTAssertTrue(logoutPill.waitForExistence(timeout: 5))
        logoutPill.firstMatch.tap()

        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 3), "confirm did not present")

        // Two "Log out" buttons can exist (settings pill + dialog commit); the
        // dialog's shares the Cancel row, so pick by vertical alignment.
        let candidates = app.buttons.matching(
            NSPredicate(format: "label == 'Log out'")
        ).allElementsBoundByIndex
        let cancelMidY = cancel.frame.midY
        guard
            let commit = candidates.first(where: { abs($0.frame.midY - cancelMidY) < 4 })
        else {
            return XCTFail("no dialog Log out button beside Cancel")
        }
        commit.tap()

        XCTAssertTrue(
            app.buttons["Scan QR"].waitForExistence(timeout: 5),
            "confirm tap produced no visible logout (landing never appeared)")
    }

    /// The overlay's reason to live at the app root: while the dialog is up,
    /// the Liquid Glass tab bar must be hit-blocked — a tab switch under the
    /// scrim would orphan the presentation.
    func testTabBarBlockedWhileConfirmUp() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-baybo-open-home", "-baybo-home-tab", "settings", "-baybo.lang", "en",
        ]
        app.launch()

        let logoutPill = app.buttons["Log out"]
        XCTAssertTrue(logoutPill.waitForExistence(timeout: 5))

        // Capture the Chats tab item's center BEFORE presenting: the card's
        // .isModal hides underlying elements from queries once it is up.
        let chatsTab = app.buttons["Chats"].firstMatch
        XCTAssertTrue(chatsTab.waitForExistence(timeout: 3), "no Chats tab item")
        let chatsCenter = CGPoint(x: chatsTab.frame.midX, y: chatsTab.frame.midY)

        logoutPill.tap()
        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 3), "confirm did not present")
        Thread.sleep(forTimeInterval: 0.6)

        // Tap the Chats tab item's own center. The scrim owns the whole field,
        // so this reads as a scrim-cancel — the dialog closes — but it must
        // NOT reach the bar: the section stays Settings.
        app.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: chatsCenter.x, dy: chatsCenter.y)).tap()
        XCTAssertTrue(cancel.waitForNonExistence(timeout: 3), "scrim tap did not cancel")
        XCTAssertTrue(
            logoutPill.waitForExistence(timeout: 2),
            "tab bar took the tap under the scrim — section switched away from Settings")
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

    /// The dead-flank regression: `OutlinePillButtonStyle` draws a stroke with
    /// no fill, and SwiftUI does not hit-test transparent pixels — without the
    /// style's explicit `contentShape(Capsule())`, only the text column of the
    /// pill was tappable (~1/4 of its width) and BOTH logout-flow buttons (the
    /// settings pill and the dialog's red commit) read as randomly dead.
    /// Drives flank taps through both, all the way to landing.
    func testPillFlankTapsDriveTheWholeLogoutFlow() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-baybo-open-home", "-baybo-home-tab", "settings", "-baybo.lang", "en",
        ]
        app.launch()

        let logoutPill = app.buttons["Log out"]
        XCTAssertTrue(logoutPill.waitForExistence(timeout: 5))

        // Left flank of the settings pill: inside the capsule, off the text.
        logoutPill.coordinate(withNormalizedOffset: CGVector(dx: 0.08, dy: 0.5)).tap()
        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(
            cancel.waitForExistence(timeout: 3),
            "settings pill flank tap dead — outline pill hit area excludes its interior")

        // Right flank of the dialog's red commit pill (beside Cancel).
        let cancelMidY = cancel.frame.midY
        let candidates = app.buttons.matching(
            NSPredicate(format: "label == 'Log out'")
        ).allElementsBoundByIndex
        guard
            let commit = candidates.first(where: { abs($0.frame.midY - cancelMidY) < 4 })
        else {
            return XCTFail("no dialog Log out button beside Cancel")
        }
        commit.coordinate(withNormalizedOffset: CGVector(dx: 0.9, dy: 0.5)).tap()

        XCTAssertTrue(
            app.buttons["Scan QR"].waitForExistence(timeout: 5),
            "commit pill flank tap dead — logout never reached landing")
    }
}
