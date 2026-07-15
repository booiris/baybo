import XCTest

/// Headless drive of the cron-group exit contract: opening a scheduled job's
/// fire drills IN through the fire list, but backing OUT of the fire lands on
/// the MAIN chat list — not the fire list it was reached through
/// (`AppStore.openCronGroupSession` resets the nav path rather than appending).
///
/// Runs against `-baybo-open-home`, which seeds one cron group ("Morning brief",
/// job `demo-job`) with two fires ("Morning brief · 7/14" / "· 7/13"). The three
/// screens are told apart by markers unique to each:
/// - main list — the `Menu` (☰) button in the list header;
/// - fire list — a fire row (`Morning brief · 7/14`), with no `Menu`;
/// - chat — the composer's `Send` button.
final class CronExitUITests: BayboUITestCase {
    private static let groupTitle = "Morning brief"
    private static let fireTitle = "Morning brief · 7/14"

    private func launchHome() -> XCUIApplication {
        launch(["-baybo-open-home"])
    }

    /// Drill list → fire list → chat, then assert the parked state is the chat.
    private func openFireChat(_ app: XCUIApplication) {
        let group = app.staticTexts[Self.groupTitle]
        XCTAssertTrue(group.waitForExistence(timeout: 5), "cron group row never appeared")
        XCTAssertTrue(app.buttons["Menu"].exists, "not on the main list before drilling in")

        group.tap()
        let fire = app.staticTexts[Self.fireTitle]
        XCTAssertTrue(fire.waitForExistence(timeout: 3), "tapping the group did not open its fire list")
        XCTAssertFalse(app.buttons["Menu"].exists, "still on the main list after opening the fire list")

        fire.tap()
        XCTAssertTrue(
            app.buttons["Send"].waitForExistence(timeout: 5),
            "tapping a fire did not open the chat")
    }

    /// The whole point: back out of a cron fire → the main list, skipping the
    /// fire list. Old behaviour (append) would land on the fire list instead.
    func testBackFromCronFireLandsOnMainList() throws {
        let app = launchHome()
        openFireChat(app)

        app.buttons["Back to conversations"].tap()

        // Main list, positively AND negatively: the ☰ menu is back, and the fire
        // list is NOT what we landed on (its rows are gone). Both together pin the
        // destination — the old append chain would satisfy neither.
        XCTAssertTrue(
            app.buttons["Menu"].waitForExistence(timeout: 3),
            "back from a cron fire did not land on the main list")
        XCTAssertFalse(
            app.staticTexts[Self.fireTitle].exists,
            "back from a cron fire landed on the fire list, not the main list")
        XCTAssertTrue(
            app.staticTexts[Self.groupTitle].exists, "the cron group row is missing from the list")
    }

    /// The reset-path pop still leaves the stack responsive: the cron group opens
    /// again and its fire pushes again after the round trip. A PopGestureEnabler
    /// that stranded the interactive-pop recognizer at the root would wedge this.
    func testStackStaysResponsiveAfterCronExit() throws {
        let app = launchHome()
        openFireChat(app)
        app.buttons["Back to conversations"].tap()
        XCTAssertTrue(app.buttons["Menu"].waitForExistence(timeout: 3), "did not return to the list")

        // Second round trip: group → fire list → chat → back → list.
        openFireChat(app)
        app.buttons["Back to conversations"].tap()
        XCTAssertTrue(
            app.buttons["Menu"].waitForExistence(timeout: 3),
            "stack dead after a cron exit — pop recognizer likely stranded")
        XCTAssertFalse(
            app.staticTexts[Self.fireTitle].exists, "second exit landed on the fire list")
    }
}
