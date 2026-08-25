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
}

/// Mirrors `IssueDock.fieldIdentifier` — the app module is not importable from
/// a UI test bundle.
enum IssueDockFields {
    static let field = "issue.field"
}
