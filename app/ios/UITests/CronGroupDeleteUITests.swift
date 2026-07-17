import XCTest

/// Headless drive of a cron group's delete: the trailing swipe, the confirm that
/// states what "delete a folder that is a view" actually does, and the commit.
///
/// Runs against `-baybo-open-home`, which seeds one group ("Morning brief", job
/// `demo-job`) with TWO fires — so the dialog's count has a value to be wrong
/// about. Session mutations resolve locally there (no gateway), so the
/// optimistic removal must stay put.
///
/// Below the seam this is `SessionIndexMutationTests`; what only XCUITest can
/// reach is up here: that the swipe reveals a Delete at all, that the dialog
/// presents over the tab-bar shell, that its stroke-only commit pill is
/// hit-testable (a stroke-only pill's dead flanks have shipped before — see
/// `LogoutConfirmUITests`), and that the count reaches the copy as a NUMBER
/// rather than a raw `%@`.
final class CronGroupDeleteUITests: BayboUITestCase {
    private static let groupTitle = "Morning brief"
    private static let fireTitle = "Morning brief · 7/14"

    private func launchHome() -> XCUIApplication {
        launch(["-baybo-open-home"])
    }

    /// Reveal the group row's parked trailing actions.
    private func revealGroupActions(_ app: XCUIApplication) {
        let group = app.staticTexts[Self.groupTitle]
        XCTAssertTrue(group.waitForExistence(timeout: 5), "cron group row never appeared")
        group.swipeLeft()
    }

    /// The dialog must name the blast radius: two seeded fires, so "2 execution
    /// records". A `%@` that never got substituted would read the same to a
    /// by-key test and be nonsense to a user — this asserts the NUMBER.
    ///
    /// Cancel is the other half: a destructive confirm that removes the rows on
    /// the way out would be worse than no confirm at all.
    func testDeleteAsksConfirmationNamingTheRunCountAndCancelKeepsTheGroup() throws {
        let app = launchHome()
        revealGroupActions(app)

        let delete = app.buttons["Delete"]
        XCTAssertTrue(delete.waitForExistence(timeout: 3), "group swipe revealed no Delete")
        delete.tap()

        XCTAssertTrue(
            app.staticTexts["Delete all runs?"].waitForExistence(timeout: 3),
            "the group delete confirm did not present")
        let body = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS %@", "all 2 execution records")
        ).firstMatch
        XCTAssertTrue(
            body.waitForExistence(timeout: 3),
            "the confirm never named the run count it will clear")

        let cancel = app.buttons["Cancel"]
        cancel.tap()
        XCTAssertTrue(cancel.waitForNonExistence(timeout: 3), "Cancel did not dismiss")
        XCTAssertTrue(
            app.staticTexts[Self.groupTitle].exists, "Cancel still deleted the group")
    }

    /// The commit clears the execution records, so the group has no visible fire
    /// left — and a group with no members has no row (it is a view over them).
    /// Its fires must be gone from the list too, not merely regrouped.
    func testDeleteConfirmClearsEveryRunAndTheGroupRow() throws {
        let app = launchHome()
        revealGroupActions(app)

        let swipeDelete = app.buttons["Delete"]
        XCTAssertTrue(swipeDelete.waitForExistence(timeout: 3))
        swipeDelete.tap()
        try dialogCommit(app, "Delete").tap()

        XCTAssertTrue(
            app.staticTexts[Self.groupTitle].waitForNonExistence(timeout: 3),
            "the confirmed delete kept the group row listed")
        XCTAssertFalse(
            app.staticTexts[Self.fireTitle].exists,
            "a fire outlived the group's delete — the batch missed a member")
        // The list itself is untouched: this deleted one job's records, not a
        // swathe of the user's conversations.
        XCTAssertTrue(
            app.staticTexts["Demo conversation number 1"].exists,
            "the group's delete took an unrelated conversation with it")
    }
}
