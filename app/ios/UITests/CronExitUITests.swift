import XCTest

/// Headless drive of the cron-group navigation: opening a scheduled job's fire
/// drills IN through the fire list, and backing OUT retraces the same chain —
/// chat → fire list → main chat list — the way the Archived screen does.
///
/// (An earlier build dropped the fire list from the back stack on entry so Back
/// would skip straight to the main list; but `NavigationStack` keys its path
/// positionally, so collapsing it remounted the chat page → a visible flash.
/// See `AppStore.openCronGroupSession`. The drill-in is now a plain push.)
///
/// Runs against `-baybo-open-home`, which seeds one cron group ("Morning brief",
/// job `demo-job`) with two fires ("Morning brief · 7/14" / "· 7/13"). The three
/// screens are told apart by markers unique to each:
/// - main list — the `Menu` (☰) button in the list header;
/// - fire list — a fire row (`Morning brief · 7/14`), with no `Menu`;
/// - chat — the composer's `Send` button.
///
/// The chat's back chevron and the fire list's back chevron carry the SAME
/// accessibility label (`chat.back` → "Back to conversations"), so only one is
/// ever on screen and a tap always pops the current top.
final class CronExitUITests: BayboUITestCase {
    private static let groupTitle = "Morning brief"
    private static let fireTitle = "Morning brief · 7/14"
    private static let backLabel = "Back to conversations"

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

    /// Back out of a cron fire: first to the fire list it was reached through,
    /// then to the main list — the standard drill-in chain, no step skipped.
    func testBackFromCronFireRetracesFireListThenMainList() throws {
        let app = launchHome()
        openFireChat(app)

        // First back: the fire list (a fire row is present, the main list's ☰ is
        // not).
        app.buttons[Self.backLabel].tap()
        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].waitForExistence(timeout: 3),
            "back from a cron fire did not land on its fire list")
        XCTAssertFalse(app.buttons["Menu"].exists, "skipped the fire list back to the main list")

        // Second back: the main list (☰ is back, the fire rows are gone).
        app.buttons[Self.backLabel].tap()
        XCTAssertTrue(
            app.buttons["Menu"].waitForExistence(timeout: 3),
            "back from the fire list did not land on the main list")
        XCTAssertFalse(
            app.staticTexts[Self.fireTitle].exists, "still on the fire list after the second back")
        XCTAssertTrue(
            app.staticTexts[Self.groupTitle].exists, "the cron group row is missing from the list")
    }

    /// The pop chain stays responsive across a full round trip: the cron group
    /// opens again and its fire pushes again after backing all the way out. A
    /// PopGestureEnabler that stranded the interactive-pop recognizer would wedge
    /// this.
    func testStackStaysResponsiveAfterCronExit() throws {
        let app = launchHome()
        backOutToMainList(app)
        XCTAssertTrue(app.buttons["Menu"].waitForExistence(timeout: 3), "did not return to the list")

        // Second round trip proves the stack + pop recognizer survived the first.
        backOutToMainList(app)
        XCTAssertTrue(
            app.buttons["Menu"].waitForExistence(timeout: 3),
            "stack dead after a cron exit — pop recognizer likely stranded")
        XCTAssertFalse(
            app.staticTexts[Self.fireTitle].exists, "second exit did not reach the main list")
    }

    /// Drill in to a fire chat, then back out the full chain (chat → fire list →
    /// main list), waiting for the intermediate fire list so the second tap can't
    /// race the first transition.
    private func backOutToMainList(_ app: XCUIApplication) {
        openFireChat(app)
        app.buttons[Self.backLabel].tap()
        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].waitForExistence(timeout: 3),
            "back from the fire chat did not reach its fire list")
        app.buttons[Self.backLabel].tap()
    }
}
