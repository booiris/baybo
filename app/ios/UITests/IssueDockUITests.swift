import UIKit
import XCTest

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

    func testTheCardDockCarriesAPlusAndASend() {
        let app = openCard()

        XCTAssertTrue(app.buttons[Self.plusLabel].exists, "the card dock has no attach button")
        XCTAssertTrue(app.buttons[Self.plusLabel].isHittable)
        XCTAssertTrue(app.buttons["issue-send"].exists)
    }

    func testThePlusPanelOpensOverTheCardsOwnRows() {
        let app = openCard()
        app.buttons[Self.plusLabel].tap()

        let photos = app.buttons[Self.photosRow]
        XCTAssertTrue(photos.waitForExistence(timeout: 3), "the + panel never opened")
        XCTAssertTrue(photos.isHittable, "something in the dock is taking the panel's taps")
        XCTAssertTrue(app.buttons[Self.filesRow].exists)
        attachScreenshot(app, name: "card-plus-panel")

        XCTAssertTrue(app.buttons[Self.plusLabel].isHittable)
        app.buttons[Self.plusLabel].tap()
        XCTAssertFalse(photos.waitForExistence(timeout: 1))
    }

    func testAFailedCommentStaysAsOneRetryableOptimisticPost() {
        let app = openCard()
        let field = app.textFields[IssueDockFields.field]
        field.tap()
        field.typeText("this will not land")

        app.buttons["issue-send"].tap()

        XCTAssertEqual(
            app.textFields[IssueDockFields.field].value as? String, "",
            "the composer waited for the network instead of handing off to the outbox")
        let retry = app.buttons["Send failed — tap to retry"]
        XCTAssertTrue(retry.waitForExistence(timeout: Self.webviewTimeout))
        XCTAssertEqual(app.staticTexts.matching(identifier: "this will not land").count, 1)
        attachScreenshot(app, name: "card-comment-failed")

        retry.tap()

        XCTAssertTrue(retry.waitForExistence(timeout: 3), "retry did not return to failed")
        XCTAssertEqual(
            app.staticTexts.matching(identifier: "this will not land").count, 1,
            "retry appended a second optimistic comment")
    }

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

        XCTAssertFalse(
            app.buttons[Self.mention("dev-2")].exists, "the strip stayed up after completing")
        XCTAssertFalse(
            offered.waitForExistence(timeout: 1), "the completed handle was offered again")
        XCTAssertEqual(
            app.textFields[IssueDockFields.field].value as? String, "@dev-1 ",
            "the draft changed on its own after the completion settled")
    }

    func testAnAddressIsNotOfferedAsAMention() {
        let app = openCard()
        let field = app.textFields[IssueDockFields.field]
        field.tap()
        field.typeText("mail me@de")

        XCTAssertFalse(
            app.buttons[Self.mention("dev-1")].waitForExistence(timeout: 1),
            "an address was offered a completion")
    }

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

    func testThePlusPanelFloatsOverTheJumpDisc() {
        let app = openCard()
        let scrollTop = app.buttons["issue-scroll-top"]
        XCTAssertTrue(
            scrollTop.waitForExistence(timeout: Self.webviewTimeout),
            "the card never exposed its scroll-to-top action")
        scrollTop.tap()
        XCTAssertTrue(
            scrollTop.waitForNonExistence(timeout: 5),
            "the card did not reach the top before checking its jump disc")

        let jump = app.buttons["issue-jump"]
        XCTAssertTrue(
            jump.waitForExistence(timeout: Self.webviewTimeout),
            "a card taller than its screen offers no way to the bottom")
        let jumpFrame = jump.frame

        app.buttons[Self.plusLabel].tap()
        XCTAssertTrue(
            app.buttons[Self.filesRow].waitForExistence(timeout: 3), "the + panel never opened")

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

    private static func mention(_ handle: String) -> String { "issue-mention.\(handle)" }
}

enum IssueDockFields {
    static let field = "issue.field"
}
