import UIKit
import XCTest

/// The card's dock, headless: the field, the send, and the `+` panel over it.
///
/// The file `IssueDock.swift` has claimed exists since P8 and did not until
/// the dock grew attachments. The demo card has NO gateway behind it, which is
/// what makes the send case worth having: every comment fails, so what this
/// asserts is the failure path — the one that must not throw away picks.
final class IssueDockUITests: BayboUITestCase {
    private static let cardArguments = [
        "-baybo-open-home", "-baybo-demo-projects", "-baybo-demo-card",
    ]
    private static let plusLabel = "Add attachment"
    private static let photosRow = "Photos"
    private static let filesRow = "Files"
    private static let pasteRow = "Paste"

    private func openCard() -> XCUIApplication {
        let app = launch(Self.cardArguments)
        XCTAssertTrue(
            app.textFields[IssueDockFields.field].waitForExistence(timeout: 10),
            "the card dock never appeared")
        return app
    }

    /// The card's field IS the chat's pill now — same 48pt row, same glass,
    /// with a `+` on the left. What a UI test can hold of that is the two
    /// controls being there and reachable.
    func testTheCardDockCarriesAPlusAndASend() {
        let app = openCard()

        XCTAssertTrue(app.buttons[Self.plusLabel].exists, "the card dock has no attach button")
        XCTAssertTrue(app.buttons[Self.plusLabel].isHittable)
        XCTAssertTrue(app.buttons["issue-send"].exists)
    }

    /// The panel floats over the DOCK's own rows — the hint line, the approval
    /// card, the staged strip. Presented from the screen's ZStack instead of
    /// the dock's layer it draws behind them and they take its taps, which is
    /// the bug `ComposerDock` exists for; `isHittable` on a row is what catches
    /// the hit-testing half of it.
    func testThePlusPanelOpensOverTheCardsOwnRows() {
        let app = openCard()
        app.buttons[Self.plusLabel].tap()

        let photos = app.buttons[Self.photosRow]
        XCTAssertTrue(photos.waitForExistence(timeout: 3), "the + panel never opened")
        XCTAssertTrue(photos.isHittable, "something in the dock is taking the panel's taps")
        XCTAssertTrue(app.buttons[Self.filesRow].exists)
        attachScreenshot(app, name: "card-plus-panel")

        // Its own scrim dismisses it, and the `+` stays live underneath — the
        // hand-rolled panel's whole difference from a system `Menu`.
        XCTAssertTrue(app.buttons[Self.plusLabel].isHittable)
        app.buttons[Self.plusLabel].tap()
        XCTAssertFalse(photos.waitForExistence(timeout: 1))
    }

    /// **A comment that did not land keeps its text.** There is no outbox on
    /// this surface: the dock used to clear the field before the write and
    /// never learn the answer, which was survivable while a comment was only
    /// words. It carries uploaded blobs now, and discarding on failure strands
    /// files the operator cannot get back.
    ///
    /// The demo has no gateway, so the comment always fails — which is exactly
    /// the case under test.
    func testAFailedCommentKeepsWhatWasTyped() {
        let app = openCard()
        let field = app.textFields[IssueDockFields.field]
        field.tap()
        field.typeText("this will not land")

        app.buttons["issue-send"].tap()

        // The banner is the report; the text staying is the point.
        XCTAssertTrue(
            app.textFields[IssueDockFields.field].value as? String == "this will not land",
            "a failed comment threw away what was typed")
    }

    /// **A half-typed `@` offers the board's roster**, and completing writes
    /// the handle the gateway will read. The strip is the only part of the
    /// card dock that reaches into UIKit for the caret, so this case is what
    /// says the reach still works — a `TextField` reports no selection, and
    /// the fallback (the end of the draft) would pass a weaker assertion.
    func testAHalfTypedHandleOffersTheBoardsRoster() {
        let app = openCard()
        let field = app.textFields[IssueDockFields.field]
        field.tap()
        field.typeText("@de")

        let offered = app.buttons[Self.mention("dev-1")]
        XCTAssertTrue(offered.waitForExistence(timeout: 3), "no mention strip appeared")
        XCTAssertTrue(app.buttons[Self.mention("dev-2")].exists)
        XCTAssertFalse(
            app.buttons[Self.mention("qa-1")].exists, "the strip offered a handle that cannot match")

        offered.tap()
        XCTAssertEqual(
            app.textFields[IssueDockFields.field].value as? String, "@dev-1 ",
            "completing left something other than the handle and one space")

        // **The strip closes and STAYS closed on the handle just chosen.** The
        // field's up-sync lands a beat after the completion and re-reads the
        // caret; parked in front of the trailing space it is back inside the
        // finished handle, which reopens the strip on `@dev-1` — one tap from
        // writing it twice. Held open a second, because that beat is the case.
        XCTAssertFalse(
            app.buttons[Self.mention("dev-2")].exists, "the strip stayed up after completing")
        XCTAssertFalse(
            offered.waitForExistence(timeout: 1), "the completed handle was offered again")
        XCTAssertEqual(
            app.textFields[IssueDockFields.field].value as? String, "@dev-1 ",
            "the draft changed on its own after the completion settled")
    }

    /// The negative control, and the reason the grammar is mirrored from
    /// `crates/project/src/mentions.rs` rather than approximated: an `@` after
    /// a letter is an address, the gateway reads no mention out of it, and a
    /// strip that offered one would promise a delivery nobody makes.
    func testAnAddressIsNotOfferedAsAMention() {
        let app = openCard()
        let field = app.textFields[IssueDockFields.field]
        field.tap()
        field.typeText("mail me@de")

        XCTAssertFalse(
            app.buttons[Self.mention("dev-1")].waitForExistence(timeout: 1),
            "an address was offered a completion")
    }

    /// The card's own assignee leads the strip — the plumbing a model test
    /// cannot see, since `IssueMention` is handed the id and this is what
    /// hands it over. `lead` is first on the demo board's roster; `dev-1` is
    /// #41's agent.
    func testTheCardsAssigneeLeadsTheStrip() {
        let app = openCard()
        let field = app.textFields[IssueDockFields.field]
        field.tap()
        field.typeText("@")

        let assignee = app.buttons[Self.mention("dev-1")]
        XCTAssertTrue(assignee.waitForExistence(timeout: 3), "a bare @ offered nobody")
        XCTAssertLessThan(
            assignee.frame.minX, app.buttons[Self.mention("lead")].frame.minX,
            "the card's agent is not the first chip")
    }

    /// **The panel covers the jump disc**, which on a card is up the moment it
    /// opens: a card is opened at the top and its Activity is at the bottom.
    /// The disc used to be a row in this dock's stack, so the panel — which
    /// hangs off the stack's top edge — opened a disc's height clear of the
    /// `+`. `ComposerAttachUITests` holds the chat's half of this.
    func testThePlusPanelFloatsOverTheJumpDisc() {
        let app = openCard()
        let jump = app.buttons["issue-jump"]
        XCTAssertTrue(
            jump.waitForExistence(timeout: Self.webviewTimeout),
            "a card taller than its screen offers no way to the bottom")
        let jumpFrame = jump.frame

        app.buttons[Self.plusLabel].tap()
        XCTAssertTrue(
            app.buttons[Self.filesRow].waitForExistence(timeout: 3), "the + panel never opened")

        // The panel's LOWEST row is the one in the disc's band, and which row
        // that is depends on the clipboard: `Paste` is offered only when there
        // is an image on it, and the simulator's board is not this suite's to
        // decide.
        let bottom = try? XCTUnwrap(
            [Self.photosRow, Self.filesRow, Self.pasteRow]
                .map { app.buttons[$0] }
                .filter(\.exists)
                .max { $0.frame.maxY < $1.frame.maxY },
            "the panel drew no rows at all")
        XCTAssertTrue(
            bottom?.frame.intersects(jumpFrame) == true,
            "the panel was pushed clear of the disc instead of covering it")
        XCTAssertTrue(
            bottom?.isHittable == true, "the disc is on top of the panel — it takes its taps")
        attachScreenshot(app, name: "card-plus-panel-over-jump")
    }

    /// Mirrors `IssueDock`'s per-handle identifier.
    private static func mention(_ handle: String) -> String { "issue-mention.\(handle)" }
}

/// Mirrors `IssueDock.fieldIdentifier` — the app module is not importable from
/// a UI test bundle.
enum IssueDockFields {
    static let field = "issue.field"
}
