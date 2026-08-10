import XCTest

/// Headless drive of the **conversation rename**: long-press → Rename →
/// `RenameDialog` → the row's bold first line changes.
///
/// What only XCUITest can reach here is the gesture chain and the surfaces that
/// carry it (`sessionContextMenu` rides three screens), plus the one thing a
/// unit test structurally cannot see: that the editor is REACHABLE and its
/// commit button lands. `RenameTitleTests` covers the rules the commit applies
/// and `SessionIndexRenameTests` what the row does with the result.
///
/// Runs against `-baybo-open-home`, where session mutations resolve locally
/// (no gateway) — so the optimistic title is expected to stay put.
final class RenameMenuUITests: BayboUITestCase {
    private static let rename = "Rename"
    private static let dialogTitle = "Rename conversation"
    private static let save = "Save"

    private func launchHome() -> XCUIApplication {
        launch(["-baybo-open-home"])
    }

    private func rowButton(_ app: XCUIApplication, containing label: String) -> XCUIElement {
        app.buttons.containing(.staticText, identifier: label).firstMatch
    }

    /// Long-press a row and tap Rename, leaving the editor up.
    private func openEditor(
        _ app: XCUIApplication, on label: String, _ surface: String,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        let row = rowButton(app, containing: label)
        XCTAssertTrue(
            row.waitForExistence(timeout: 5), "\(surface): the row never appeared",
            file: file, line: line)
        row.press(forDuration: 1.1)

        let item = app.buttons[Self.rename]
        XCTAssertTrue(
            item.waitForExistence(timeout: 3),
            "\(surface): long-press offered no Rename — the row is missing sessionContextMenu",
            file: file, line: line)
        item.tap()

        XCTAssertTrue(
            app.staticTexts[Self.dialogTitle].waitForExistence(timeout: 3),
            "\(surface): Rename did not raise the editor", file: file, line: line)
    }

    /// The field, seeded with what the row shows.
    private func editorField(_ app: XCUIApplication) throws -> XCUIElement {
        let field = app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 3), "the editor has no field")
        return field
    }

    /// Replace the whole field. Backspaces rather than the edit menu's Select
    /// All: that menu is system chrome whose presence and labels are not ours,
    /// and when it fails to appear the typing lands APPENDED to the seed — a
    /// green-looking test that renamed a conversation to something nobody asked
    /// for. The dialog focuses the field itself and parks the caret at the end,
    /// so this needs no tap (one would move the caret into the middle).
    private func replaceText(_ field: XCUIElement, with text: String) {
        let existing = (field.value as? String) ?? ""
        field.typeText(
            String(repeating: XCUIKeyboardKey.delete.rawValue, count: existing.count) + text)
    }

    /// Main list → ☰ → Archived.
    private func openArchived(_ app: XCUIApplication) {
        XCTAssertTrue(app.buttons["Menu"].waitForExistence(timeout: 5), "list header never appeared")
        app.buttons["Menu"].tap()
        let entry = app.buttons["Archived"]
        XCTAssertTrue(entry.waitForExistence(timeout: 3), "menu did not open")
        entry.tap()
    }

    /// The whole gesture, end to end, on the surface it is reached from most.
    func testChatListRowRenames() throws {
        let app = launchHome()
        openEditor(app, on: "Ship the iOS chat list", "chat list")

        let field = try editorField(app)
        // The editor is transient chrome — this is the only way to see it from
        // outside the runner, and the one thing worth looking at is that the
        // card clears the keyboard it raises.
        attachScreenshot(app, name: "rename-editor")
        // The seed IS the current title: a rename that had to be typed from
        // scratch would be a different (worse) feature.
        XCTAssertEqual(field.value as? String, "Ship the iOS chat list")

        replaceText(field, with: "Renamed from iOS")
        app.buttons[Self.save].tap()

        XCTAssertTrue(
            app.staticTexts["Renamed from iOS"].waitForExistence(timeout: 3),
            "the row's headline did not take the new title")
        XCTAssertFalse(
            app.staticTexts["Ship the iOS chat list"].exists, "the old title is still listed")
    }

    /// Cancel must leave the title alone — the editor is the one dialog here
    /// that carries a draft, so backing out has to discard it.
    ///
    /// Driven from the archived screen, which doubles as that surface's coverage:
    /// archiving a conversation is a filing decision, not a decision to stop
    /// being able to name it.
    func testCancelOnAnArchivedRowKeepsTheOldTitle() throws {
        let app = launchHome()
        openArchived(app)
        openEditor(app, on: "Weekend trip planning", "archived screen")

        let field = try editorField(app)
        replaceText(field, with: "Discarded draft")
        app.buttons["Cancel"].tap()

        XCTAssertTrue(
            app.staticTexts["Weekend trip planning"].waitForExistence(timeout: 3),
            "Cancel changed the title")
        XCTAssertFalse(app.staticTexts["Discarded draft"].exists, "Cancel committed the draft")
    }

    /// A cron fire is where a hand-written name earns the most — every fire of a
    /// job is minted with a title built from the same job name — and its list is
    /// the only place one is shown.
    func testCronFireOffersRename() throws {
        let app = launchHome()
        let group = app.staticTexts["Morning brief"]
        XCTAssertTrue(group.waitForExistence(timeout: 5), "cron group row never appeared")
        group.tap()

        openEditor(app, on: "Morning brief · 7/14", "cron fire")
        app.buttons["Cancel"].tap()
        XCTAssertTrue(
            app.staticTexts["Morning brief · 7/14"].waitForExistence(timeout: 3),
            "the fire left the list")
    }

    /// An empty field cannot be committed: the gateway answers 400 for it, and
    /// there is deliberately no "clear it and let the model re-title" — an absent
    /// `SessionPatch.title` already means "unchanged" on the wire.
    func testAnEmptyTitleCannotBeSaved() throws {
        let app = launchHome()
        openEditor(app, on: "Refactor the sync loop", "chat list")

        let field = try editorField(app)
        replaceText(field, with: "")

        XCTAssertFalse(
            app.buttons[Self.save].isEnabled, "an empty title left the commit button live")
    }
}
