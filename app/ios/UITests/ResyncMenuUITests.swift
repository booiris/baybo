import XCTest

/// Headless drive of the **per-session resync** long-press
/// (`resyncContextMenu` → `RootView`'s confirm) on every screen that lists a
/// conversation.
///
/// The hatch shipped wired to `ChatListScreen.chatRow` alone, so a cron fire —
/// listed only inside `CronGroupScreen`, and the long, unattended, tool-heavy
/// kind of thread the hatch exists for — was the one conversation with no way to
/// reach it. The archived screen had the same hole. What only XCUITest can reach
/// is exactly that: which SURFACES carry the gesture. `ChatStoreResyncTests`
/// covers what the commit then does.
///
/// Runs against `-baybo-open-home`: six demo conversations (demo-2 archived) and
/// one cron group ("Morning brief", job `demo-job`) with two fires. Session
/// mutations resolve locally there, and the resync's own two steps (drop the
/// mirror, reload the page if it is showing) take nothing away — so on every
/// surface the row must still be listed afterwards.
final class ResyncMenuUITests: BayboUITestCase {
    private static let groupTitle = "Morning brief"
    private static let fireTitle = "Morning brief · 7/14"
    private static let confirmTitle = "Rebuild this conversation?"
    private static let resync = "Resync"

    private func launchHome() -> XCUIApplication {
        launch(["-baybo-open-home"])
    }

    /// The whole row button wrapping a line of row text. Press the BUTTON, not
    /// the `StaticText`: the context menu is attached to the button, and on a
    /// titled row the preview text sits low in the cell.
    private func rowButton(_ app: XCUIApplication, containing label: String) -> XCUIElement {
        app.buttons.containing(.staticText, identifier: label).firstMatch
    }

    /// Long-press a row and return its menu's Resync item, asserting it exists.
    /// A row with no `.contextMenu` opens no menu at all, which is the failure
    /// this whole file is about.
    private func openResyncMenu(
        _ app: XCUIApplication, on label: String, _ surface: String,
        file: StaticString = #filePath, line: UInt = #line
    ) -> XCUIElement {
        let row = rowButton(app, containing: label)
        XCTAssertTrue(
            row.waitForExistence(timeout: 5), "\(surface): the row never appeared",
            file: file, line: line)
        row.press(forDuration: 1.1)

        let item = app.buttons[Self.resync]
        XCTAssertTrue(
            item.waitForExistence(timeout: 3),
            "\(surface): long-press offered no Resync — the row is missing resyncContextMenu",
            file: file, line: line)
        return item
    }

    /// Drill main list → the seeded job's fire list.
    private func openFireList(_ app: XCUIApplication) {
        let group = app.staticTexts[Self.groupTitle]
        XCTAssertTrue(group.waitForExistence(timeout: 5), "cron group row never appeared")
        group.tap()
        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].waitForExistence(timeout: 3),
            "tapping the group did not open its fire list")
    }

    /// Main list → ☰ → Archived.
    private func openArchived(_ app: XCUIApplication) {
        XCTAssertTrue(app.buttons["Menu"].waitForExistence(timeout: 5), "list header never appeared")
        app.buttons["Menu"].tap()
        let entry = app.buttons["Archived"]
        XCTAssertTrue(entry.waitForExistence(timeout: 3), "menu did not open")
        entry.tap()
    }

    /// The bug as reported: a cron fire could not be resynced. The gesture must
    /// reach the confirm, the confirm must commit, and the fire must survive it
    /// — this is the one commit in `RootView` that takes nothing away.
    func testCronFireLongPressResyncsAndKeepsTheFire() throws {
        let app = launchHome()
        openFireList(app)

        openResyncMenu(app, on: Self.fireTitle, "cron fire").tap()
        XCTAssertTrue(
            app.staticTexts[Self.confirmTitle].waitForExistence(timeout: 3),
            "the resync confirm did not present over the pushed fire list")
        try dialogCommit(app, Self.resync).tap()

        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].waitForExistence(timeout: 3),
            "the resync removed the fire — it must discard the transcript, not the row")
    }

    /// Cancel is the other half, and the reason the confirm is not red: it must
    /// leave the fire list exactly as it found it.
    func testCronFireResyncCancelChangesNothing() throws {
        let app = launchHome()
        openFireList(app)

        openResyncMenu(app, on: Self.fireTitle, "cron fire").tap()
        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 3), "the resync confirm did not present")
        cancel.tap()

        XCTAssertTrue(cancel.waitForNonExistence(timeout: 3), "Cancel did not dismiss")
        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].exists, "Cancel still took the fire off the list")
    }

    /// Archiving is a filing decision, not a decision to stop being able to
    /// repair the transcript.
    func testArchivedRowLongPressOffersResync() throws {
        let app = launchHome()
        openArchived(app)

        openResyncMenu(app, on: "Demo conversation number 2", "archived screen").tap()
        XCTAssertTrue(
            app.staticTexts[Self.confirmTitle].waitForExistence(timeout: 3),
            "the resync confirm did not present over the archived screen")
        try dialogCommit(app, Self.resync).tap()

        XCTAssertTrue(
            app.staticTexts["Demo conversation number 2"].waitForExistence(timeout: 3),
            "the resync unarchived or removed the row")
    }

    /// The surface the gesture started on, re-asserted after being lifted into a
    /// shared modifier: an extraction that quietly drops its original call site
    /// is the classic way to trade one hole for another.
    func testChatListRowStillOffersResync() throws {
        let app = launchHome()

        openResyncMenu(app, on: "Demo conversation number 1", "chat list").tap()
        XCTAssertTrue(
            app.staticTexts[Self.confirmTitle].waitForExistence(timeout: 3),
            "the resync confirm did not present from the chat list")
        try dialogCommit(app, Self.resync).tap()

        XCTAssertTrue(
            app.staticTexts["Demo conversation number 1"].waitForExistence(timeout: 3),
            "the resync removed the row from the chat list")
    }
}
