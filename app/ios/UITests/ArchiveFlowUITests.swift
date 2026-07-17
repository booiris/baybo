import XCTest

/// Headless drive of the chat-list archive/delete surface: swipe actions,
/// full-swipe commit, the undo toast, the delete confirm, the ☰ menu, and the
/// archived screen. Runs against `-baybo-open-home` demo rows, where session
/// mutations resolve locally (no gateway) — the optimistic flip must stay put.
///
/// `BayboUITestCase.launch` wipes the device stores first, so every case starts
/// from the same seeded list regardless of what the previous one did to it.
final class ArchiveFlowUITests: BayboUITestCase {
    private func launchHome() -> XCUIApplication {
        launch(["-baybo-open-home"])
    }

    private func row(_ app: XCUIApplication, _ n: Int) -> XCUIElement {
        app.staticTexts["Demo conversation number \(n)"]
    }

    /// The whole row button that wraps a preview line. Its frame spans both
    /// lines of a titled row, so a coordinate drag anchored at its centre lands
    /// between the two text lines — unlike the preview `StaticText`, which on a
    /// titled row sits on the low second line where a full-swipe commit doesn't
    /// register.
    private func rowButton(_ app: XCUIApplication, _ n: Int) -> XCUIElement {
        app.buttons.containing(.staticText, identifier: "Demo conversation number \(n)").firstMatch
    }

    /// A stock swipe reveals the native trailing actions and parks them open
    /// (it doesn't cross the full-swipe threshold).
    private func reveal(_ element: XCUIElement) {
        element.swipeLeft()
    }

    /// Drag well past the row's leading edge — the full-swipe that commits the
    /// edge-most action (archive on the main list, unarchive when archived).
    /// The leftward travel is ABSOLUTE, not normalized to the target's width: a
    /// titled row's swipe target is its preview line, only as wide as its
    /// glyphs, so a width-normalized drag under-travels and merely reveals the
    /// actions. A fixed 600pt drag clears the commit threshold on any row shape.
    private func fullSwipe(_ element: XCUIElement) {
        let start = element.coordinate(withNormalizedOffset: CGVector(dx: 0.9, dy: 0.5))
        let end = start.withOffset(CGVector(dx: -600, dy: 0))
        start.press(forDuration: 0.05, thenDragTo: end)
    }

    func testMenuOpensArchivedScreenListingArchivedRow() throws {
        let app = launchHome()
        XCTAssertTrue(row(app, 1).waitForExistence(timeout: 5))
        XCTAssertFalse(row(app, 2).exists, "archived demo-2 leaked into the main list")

        app.buttons["Menu"].tap()
        let entry = app.buttons["Archived"]
        XCTAssertTrue(entry.waitForExistence(timeout: 3), "menu did not open")
        entry.tap()

        XCTAssertTrue(
            row(app, 2).waitForExistence(timeout: 3),
            "archived screen does not list the archived row")
        XCTAssertFalse(row(app, 1).exists, "unarchived row leaked into the archived screen")

        app.buttons["Back to conversations"].tap()
        XCTAssertTrue(
            row(app, 1).waitForExistence(timeout: 3), "back did not pop to the main list")
    }

    func testSwipeRevealsActionsAndUndoRestores() throws {
        let app = launchHome()
        let target = row(app, 6)
        XCTAssertTrue(target.waitForExistence(timeout: 5))
        reveal(target)

        let archive = app.buttons["Archive"]
        XCTAssertTrue(archive.waitForExistence(timeout: 3), "swipe revealed no Archive")
        XCTAssertTrue(app.buttons["Delete"].exists, "swipe revealed no Delete")

        archive.tap()
        // Toast first: it lives only 3s, and row-nonexistence is a stable state
        // that can wait; querying it first would eat the toast's whole window.
        let undo = app.buttons["Undo"]
        XCTAssertTrue(undo.waitForExistence(timeout: 2), "no undo toast after archive")
        // Instant single-snapshot check: the optimistic flip has already landed
        // by the time the toast is up, and a wait here would push the undo tap
        // past the toast's 3s window.
        XCTAssertFalse(target.exists, "archived row stayed in the list")
        undo.tap()
        XCTAssertTrue(target.waitForExistence(timeout: 3), "undo did not restore the row")
        XCTAssertTrue(
            undo.waitForNonExistence(timeout: 2), "toast lingered after undo")
    }

    func testFullSwipeCommitsArchive() throws {
        let app = launchHome()
        let target = row(app, 4)
        XCTAssertTrue(target.waitForExistence(timeout: 5))

        fullSwipe(rowButton(app, 4))
        XCTAssertTrue(
            app.buttons["Undo"].waitForExistence(timeout: 3),
            "no undo toast after a full-swipe archive")
        XCTAssertTrue(
            target.waitForNonExistence(timeout: 3), "full swipe did not archive the row")

        // Leave demo-4 archived; the toast self-dismisses. Verify it does.
        XCTAssertTrue(
            app.buttons["Undo"].waitForNonExistence(timeout: 5),
            "undo toast never auto-dismissed")
    }

    func testDeleteAsksConfirmationAndCancelKeepsRow() throws {
        let app = launchHome()
        let target = row(app, 5)
        XCTAssertTrue(target.waitForExistence(timeout: 5))
        reveal(target)

        let delete = app.buttons["Delete"]
        XCTAssertTrue(delete.waitForExistence(timeout: 3))
        delete.tap()

        let title = app.staticTexts["Delete this conversation?"]
        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(title.waitForExistence(timeout: 3), "delete confirm did not present")
        XCTAssertTrue(cancel.exists)

        cancel.tap()
        XCTAssertTrue(cancel.waitForNonExistence(timeout: 3), "Cancel did not dismiss")
        XCTAssertTrue(row(app, 5).exists, "Cancel still removed the row")
    }

    func testDeleteConfirmRemovesRow() throws {
        let app = launchHome()
        let target = row(app, 5)
        XCTAssertTrue(target.waitForExistence(timeout: 5))
        reveal(target)

        let swipeDelete = app.buttons["Delete"]
        XCTAssertTrue(swipeDelete.waitForExistence(timeout: 3))
        swipeDelete.tap()

        try dialogCommit(app, "Delete").tap()

        XCTAssertTrue(
            target.waitForNonExistence(timeout: 3), "confirmed delete kept the row listed")
    }

    /// The deepest push chain (list → archived → chat) stacks two
    /// PopGestureEnabler hosts. Popping all the way out must leave the nav
    /// stack responsive: a host that stranded the interactive-pop recognizer
    /// (delegate nil'd, armed at the root) would wedge the next push. Assert
    /// the menu still opens and a row still pushes after the round trip.
    func testDeepNavChainLeavesStackResponsive() throws {
        let app = launchHome()
        XCTAssertTrue(row(app, 1).waitForExistence(timeout: 5))

        app.buttons["Menu"].tap()
        let entry = app.buttons["Archived"]
        XCTAssertTrue(entry.waitForExistence(timeout: 3))
        entry.tap()

        let archivedRow = row(app, 2)
        XCTAssertTrue(archivedRow.waitForExistence(timeout: 3))
        archivedRow.tap()

        // ChatScreen pushed over the archived list: its back chevron shares the
        // "Back to conversations" label. Pop twice, chat → archived → list.
        let back = app.buttons["Back to conversations"]
        XCTAssertTrue(back.waitForExistence(timeout: 5), "chat did not push from the archived row")
        back.firstMatch.tap()
        XCTAssertTrue(
            archivedRow.waitForExistence(timeout: 3), "did not pop back to the archived screen")
        app.buttons["Back to conversations"].firstMatch.tap()
        XCTAssertTrue(row(app, 1).waitForExistence(timeout: 3), "did not pop back to the list")

        // The stack must still be live: the menu opens and a row still pushes.
        app.buttons["Menu"].tap()
        XCTAssertTrue(
            app.buttons["Archived"].waitForExistence(timeout: 3),
            "menu dead after the deep nav chain — pop recognizer likely stranded")
        app.buttons["Archived"].tap()
        XCTAssertTrue(
            row(app, 2).waitForExistence(timeout: 3), "archived screen dead after the round trip")
    }

    func testLeadingSwipePinsAndUnpins() throws {
        let app = launchHome()
        // demo-3 is seeded pinned, so it sorts to the top block.
        XCTAssertTrue(row(app, 3).waitForExistence(timeout: 5))

        // Pin an unpinned row via the leading (right) swipe.
        let target = row(app, 5)
        XCTAssertTrue(target.exists)
        target.swipeRight()
        let pin = app.buttons["Pin"]
        XCTAssertTrue(pin.waitForExistence(timeout: 3), "leading swipe revealed no Pin")
        pin.tap()

        // The row is now pinned: its leading swipe offers Unpin instead.
        target.swipeRight()
        let unpin = app.buttons["Unpin"]
        XCTAssertTrue(unpin.waitForExistence(timeout: 3), "pinned row did not offer Unpin")
        unpin.tap()

        // Unpinned again: the swipe reverts to Pin.
        target.swipeRight()
        XCTAssertTrue(
            app.buttons["Pin"].waitForExistence(timeout: 3), "unpinned row did not revert to Pin")
    }

    func testUnarchiveMovesRowBackToMainList() throws {
        let app = launchHome()
        XCTAssertTrue(row(app, 1).waitForExistence(timeout: 5))

        app.buttons["Menu"].tap()
        let entry = app.buttons["Archived"]
        XCTAssertTrue(entry.waitForExistence(timeout: 3))
        entry.tap()

        let archivedRow = row(app, 2)
        XCTAssertTrue(archivedRow.waitForExistence(timeout: 3))
        reveal(archivedRow)

        let unarchive = app.buttons["Unarchive"]
        XCTAssertTrue(unarchive.waitForExistence(timeout: 3), "swipe revealed no Unarchive")
        unarchive.tap()
        XCTAssertTrue(
            archivedRow.waitForNonExistence(timeout: 3), "unarchived row stayed archived")

        app.buttons["Back to conversations"].tap()
        XCTAssertTrue(
            row(app, 2).waitForExistence(timeout: 3),
            "unarchived row missing from the main list")
    }
}
