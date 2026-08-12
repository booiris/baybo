import XCTest

/// Headless drive of the **per-session resync** long-press
/// (`sessionContextMenu` → `AppStore.requestResync`) on every screen that lists
/// a conversation.
///
/// The hatch shipped wired to `ChatListScreen.chatRow` alone, so a cron fire —
/// listed only inside `CronGroupScreen`, and the long, unattended, tool-heavy
/// kind of thread the hatch exists for — was the one conversation with no way to
/// reach it. The archived screen had the same hole. What only XCUITest can reach
/// is exactly that: which SURFACES carry the gesture. `ChatStoreResyncTests`
/// covers what the commit then does.
///
/// It commits straight off the menu row — there is no confirm in front of it,
/// deliberately: the resync takes nothing away that the gateway cannot hand
/// back. So the assertion after every tap is that the ROW SURVIVED, which is
/// also what a confirm-less destructive action would fail.
///
/// Runs against `-baybo-open-home`: six demo conversations (demo-2 archived) and
/// one cron group ("Morning brief", job `demo-job`) with two fires.
final class ResyncMenuUITests: BayboUITestCase {
    private static let groupTitle = "Morning brief"
    private static let fireTitle = "Morning brief · 7/14"
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
            "\(surface): long-press offered no Resync — the row is missing sessionContextMenu",
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
    /// reach the commit, and the fire must survive it.
    func testCronFireLongPressResyncsAndKeepsTheFire() throws {
        let app = launchHome()
        openFireList(app)

        openResyncMenu(app, on: Self.fireTitle, "cron fire").tap()

        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].waitForExistence(timeout: 3),
            "the resync removed the fire — it must discard the transcript, not the row")
    }

    /// Dismissing the menu without choosing anything must leave the list exactly
    /// as it was. With the confirm gone this is the only remaining "back out"
    /// path, so it is the one worth pinning.
    func testDismissingTheMenuChangesNothing() throws {
        let app = launchHome()
        openFireList(app)

        let item = openResyncMenu(app, on: Self.fireTitle, "cron fire")
        // The dimmed backdrop, well below both the lifted row (the list's first)
        // and the menu that hangs under it.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.92)).tap()

        XCTAssertTrue(item.waitForNonExistence(timeout: 3), "the context menu never dismissed")
        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].exists, "dismissing the menu took the fire off the list")
    }

    /// Archiving is a filing decision, not a decision to stop being able to
    /// repair the transcript.
    func testArchivedRowLongPressOffersResync() throws {
        let app = launchHome()
        openArchived(app)

        openResyncMenu(app, on: "Demo conversation number 2", "archived screen").tap()

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
            app.staticTexts["Demo conversation number 1"].waitForExistence(timeout: 3),
            "the resync removed the row from the chat list")
    }
}
