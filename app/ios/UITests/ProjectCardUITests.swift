import UIKit
import XCTest

final class ProjectCardUITests: BayboUITestCase {
    private static let cardArguments = [
        "-baybo-open-home", "-baybo-demo-projects", "-baybo-demo-card",
    ]
    private static let title = "the dial loop drops its subscription"
    private static let statusChip = "In Progress"
    private static let priorityChip = "Urgent"
    private static let assigneeChip = "@dev-1"

    private func openBoard() -> XCUIApplication {
        launch(["-baybo-open-home", "-baybo-demo-projects", "-baybo-demo-board"])
    }

    private func openCard() -> XCUIApplication {
        let app = launch(Self.cardArguments)
        XCTAssertTrue(
            app.staticTexts[Self.title].waitForExistence(timeout: Self.webviewTimeout),
            "the card page never rendered its title")
        return app
    }

    func testTheCardReadsTitleThenTextThenState() {
        let app = openCard()

        let title = app.staticTexts[Self.title]
        let opened = app.staticTexts.containing(
            NSPredicate(format: "label BEGINSWITH 'opened by'")
        ).firstMatch
        let body = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'stops resubscribing'")
        ).firstMatch
        let status = app.buttons[Self.statusChip]

        XCTAssertTrue(opened.exists, "the provenance line is missing from the head")
        XCTAssertTrue(body.exists, "the description never rendered")
        XCTAssertTrue(status.exists, "the status chip is missing")

        XCTAssertLessThan(title.frame.maxY, opened.frame.minY, "the head is out of order")
        XCTAssertLessThan(
            opened.frame.maxY, body.frame.minY,
            "the description must follow the line saying who opened the card")
        XCTAssertLessThan(
            body.frame.maxY, status.frame.minY,
            "the chips are back above the description")
        attachScreenshot(app, name: "card-head")
    }

    func testTheStateChipsAreActuallyPainted() throws {
        let app = openCard()
        let urgent = app.buttons[Self.priorityChip]
        let assignee = app.buttons[Self.assigneeChip]
        XCTAssertTrue(urgent.exists, "no priority chip")
        XCTAssertTrue(assignee.exists, "no assignee chip")

        let shot = try XCTUnwrap(screenPixels(), "could not read the screen")
        let inked = shot.redCoverage(in: urgent.frame)
        let plain = shot.redCoverage(in: assignee.frame)

        XCTAssertGreaterThan(
            inked, 0.01,
            "the Urgent chip paints no red — the hue table never reached the page")
        XCTAssertLessThan(
            plain, 0.005,
            "the assignee chip is tinted; the hue is supposed to be keyed by a STATE")
    }

    func testACardWithNoRunsHasNoMenu() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["stage-todo"].waitForExistence(timeout: 8), "no stage bar")
        app.buttons["stage-todo"].tap()

        let row = app.buttons["issue-row-44"]
        XCTAssertTrue(row.waitForExistence(timeout: 5), "no card without runs on the board")
        row.tap()

        XCTAssertTrue(
            app.staticTexts["retire the old pump tee"].waitForExistence(
                timeout: Self.webviewTimeout),
            "the card never opened")
        XCTAssertFalse(
            app.buttons["issue-menu"].exists,
            "a card with no runs draws a ⋯ that opens nothing")
    }

    func testTheRunLogLivesInTheCardsMenu() {
        let app = openCard()
        app.buttons["issue-menu"].tap()

        let runs = app.buttons["Runs"]
        XCTAssertTrue(runs.waitForExistence(timeout: 3), "the ⋯ offers no run log")
        runs.tap()

        let live = app.buttons["#3 · Working · @dev-1"]
        XCTAssertTrue(live.waitForExistence(timeout: 3), "the live run is not in the log")
        attachScreenshot(app, name: "card-runs-menu")

        let failed = app.buttons.containing(
            NSPredicate(format: "label BEGINSWITH '#2 · Failed · @dev-2'")
        ).firstMatch
        XCTAssertTrue(failed.exists, "a settled attempt is missing from the log")
        XCTAssertTrue(
            failed.label.contains("the sandbox exited 137"),
            "a failed attempt does not say what went wrong; got \(failed.label)")
        XCTAssertLessThan(
            live.frame.minY, failed.frame.minY, "the log must read newest first")

        let neverRan = app.buttons["#1 · Cancelled · @lead"]
        XCTAssertTrue(neverRan.exists, "an attempt that never started was hidden")
        XCTAssertFalse(
            neverRan.isEnabled,
            "an attempt with no session has no transcript and must not offer one")
    }

    func testTheStatusChipMovesTheCard() {
        let app = openCard()
        app.buttons[Self.statusChip].tap()

        let review = app.buttons["move-review"]
        XCTAssertTrue(review.waitForExistence(timeout: 3), "the status chip opened no Move sheet")
        attachScreenshot(app, name: "card-move-sheet")
        XCTAssertTrue(
            review.label.contains("Review"), "the Move sheet lost its rows; got \(review.label)")
        review.tap()

        XCTAssertTrue(
            app.buttons["Review"].waitForExistence(timeout: 5),
            "the card still reads In Progress after moving it to Review")
        XCTAssertFalse(app.buttons[Self.statusChip].exists)
    }

    func testThePriorityChipSetsTheLevel() {
        let app = openCard()
        app.buttons[Self.priorityChip].tap()

        let high = app.buttons["priority-high"]
        XCTAssertTrue(high.waitForExistence(timeout: 3), "the priority chip opened no picker")
        attachScreenshot(app, name: "card-priority-picker")
        high.tap()

        XCTAssertTrue(
            app.buttons["High"].waitForExistence(timeout: 5),
            "the card still reads Urgent after choosing High")
    }

    func testTheAssigneeChipHandsTheCardOver() {
        let app = openCard()
        app.buttons[Self.assigneeChip].tap()

        let other = app.buttons["assign-dev-2"]
        XCTAssertTrue(other.waitForExistence(timeout: 3), "the assignee chip opened no picker")
        other.tap()

        XCTAssertTrue(
            app.buttons["@dev-2"].waitForExistence(timeout: 5),
            "the card still reads @dev-1 after handing it to @dev-2")
    }

    // MARK: - The dock

    func testTheFieldMatchesTheChatsRestingGeometry() {
        let chat = launch(["-baybo-open-chat"])
        let chatField = chat.descendants(matching: .any)["composer.field"].firstMatch
        XCTAssertTrue(
            chatField.waitForExistence(timeout: Self.webviewTimeout), "no chat composer")
        let chatFrame = chatField.frame
        chat.terminate()

        let card = openCard()
        let cardField = card.descendants(matching: .any)[IssueDockFields.field].firstMatch
        XCTAssertTrue(cardField.waitForExistence(timeout: 10), "no card composer")

        XCTAssertEqual(
            cardField.frame.minX, chatFrame.minX, accuracy: 0.5,
            "the card's field starts at a different gutter than the chat's")
        XCTAssertEqual(
            cardField.frame.width, chatFrame.width, accuracy: 0.5,
            "the card's field is a different width than the chat's")
        XCTAssertEqual(
            cardField.frame.maxY, chatFrame.maxY, accuracy: 0.5,
            "the card's field sits at a different height off the floor than the chat's")
    }

    func testFocusingTheFieldWidensThePill() {
        let app = openCard()
        let field = app.descendants(matching: .any)[IssueDockFields.field].firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 10), "no card composer")
        let resting = field.frame.width

        field.tap()
        XCTAssertTrue(
            app.keyboards.firstMatch.waitForExistence(timeout: 5), "the field never focused")
        XCTAssertEqual(
            field.frame.width, resting + 52, accuracy: 1,
            "focusing the card's field did not stretch the pill the way the chat's does")
    }

    func testTheFieldCarriesNoPlaceholder() {
        let app = openCard()
        let field = app.descendants(matching: .any)[IssueDockFields.field].firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 10), "no card composer")

        XCTAssertTrue(
            (field.placeholderValue ?? "").isEmpty,
            "the card's field still draws a placeholder: \(field.placeholderValue ?? "")")
        XCTAssertEqual(
            field.label, "Say something on this card",
            "a field with no placeholder needs a name of its own")
    }

    func testTheJumpDiscTakesTheCardToItsNewestActivity() {
        let app = openCard()
        let jump = app.buttons["issue-jump"]
        XCTAssertTrue(
            jump.waitForExistence(timeout: Self.webviewTimeout),
            "a card taller than its screen offers no way to the bottom")
        attachScreenshot(app, name: "card-jump-disc")

        let last = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'once the fence lands'")
        ).firstMatch
        jump.tap()

        XCTAssertTrue(
            last.waitForExistence(timeout: 10), "the jump landed nowhere near the newest comment")
        XCTAssertTrue(
            jump.waitForNonExistence(timeout: 5),
            "the disc stayed up after taking the card to the bottom")
    }

    func testJumpingWhileTheFieldIsFocusedDoesNotLeaveAKeyboardSizedGap() {
        let app = openCard()
        let field = app.textFields[IssueDockFields.field]
        field.tap()
        XCTAssertTrue(
            app.keyboards.firstMatch.waitForExistence(timeout: 5),
            "the keyboard never covered the card")

        let jump = app.buttons["issue-jump"]
        XCTAssertTrue(
            jump.waitForExistence(timeout: Self.webviewTimeout),
            "the focused card offered no way to its newest activity")
        jump.tap()

        let last = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'once the fence lands'")
        ).firstMatch
        XCTAssertTrue(last.waitForExistence(timeout: 10), "the jump did not reveal the last post")
        XCTAssertTrue(jump.waitForNonExistence(timeout: 5), "the page did not reach its bottom")

        let gap = field.frame.minY - last.frame.maxY
        attachScreenshot(app, name: "card-focused-jump-bottom")
        XCTAssertGreaterThanOrEqual(gap, 0, "the dock still covers the final post")
        XCTAssertLessThan(
            gap, 140,
            "the keyboard was counted twice, leaving a \(gap)-point blank tail")
    }

    func testTheCardsWordsCanBeCopied() {
        let app = openCard()
        let body = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'stops resubscribing'")
        ).firstMatch
        XCTAssertTrue(body.waitForExistence(timeout: Self.webviewTimeout))

        body.press(forDuration: 1.0)

        XCTAssertTrue(
            app.menuItems["Copy"].waitForExistence(timeout: 5)
                || app.buttons["Copy"].waitForExistence(timeout: 1),
            "a long press on the description raised no Copy — the page is still inert")
    }

    func testTheLastCommentCanBeScrolledClearOfTheDock() {
        let app = openCard()
        let last = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'once the fence lands'")
        ).firstMatch
        XCTAssertTrue(
            last.waitForExistence(timeout: Self.webviewTimeout),
            "the card's last comment never rendered")

        let started = last.frame.minY
        var previous = CGFloat.greatestFiniteMagnitude
        for _ in 0..<8 {
            let y = last.frame.minY
            if abs(y - previous) < 1 { break }
            previous = y
            app.swipeUp()
        }
        XCTAssertLessThan(
            last.frame.minY, started - 20,
            "the demo card fits the viewport, so nothing here was under test")

        let field = app.textFields[IssueDockFields.field]
        XCTAssertTrue(field.exists, "the dock never appeared")
        attachScreenshot(app, name: "card-bottom")
        XCTAssertLessThan(
            last.frame.maxY, field.frame.minY,
            "the card's last comment cannot be scrolled out from under the dock")
    }

    func testAHorizontalDragMovesNothing() {
        let app = openCard()
        let title = app.staticTexts[Self.title]
        let before = title.frame

        app.swipeLeft()
        XCTAssertEqual(
            title.frame.minX, before.minX, accuracy: 0.5,
            "the card panned sideways — the page is scrolling in two axes again")
        app.swipeRight()
        XCTAssertEqual(title.frame.minX, before.minX, accuracy: 0.5, "the card panned back")
    }

    func testThePagesOwnOpenRunLinkOpensTheRun() {
        let app = openCard()
        let link = app.buttons.containing(
            NSPredicate(format: "label CONTAINS 'Open run'")
        ).firstMatch
        XCTAssertTrue(
            link.waitForExistence(timeout: Self.webviewTimeout),
            "the live-run line offers no way into the run")
        link.tap()

        XCTAssertTrue(
            app.staticTexts["#41 · attempt 3"].waitForExistence(timeout: 8),
            "the page's Open run opened no run sheet")
        attachScreenshot(app, name: "card-open-run")
    }

    // MARK: - Leaving and coming back

    func testNestedSubIssuesRestoreBothCoveredCards() throws {
        let app = openCard()
        let child = app.buttons.containing(
            NSPredicate(format: "label CONTAINS '#42'")
        ).firstMatch
        XCTAssertTrue(child.waitForExistence(timeout: Self.webviewTimeout), "no sub-issue row")
        child.tap()

        XCTAssertTrue(
            app.staticTexts["keepalive should feed liveness, not the timer"].waitForExistence(
                timeout: Self.webviewTimeout),
            "the sub-issue never opened")

        let grandchild = app.buttons.containing(
            NSPredicate(format: "label CONTAINS '#43'")
        ).firstMatch
        XCTAssertTrue(
            grandchild.waitForExistence(timeout: Self.webviewTimeout),
            "the child card has no nested sub-issue row")
        grandchild.tap()

        XCTAssertTrue(
            app.staticTexts["write the connection doc"].waitForExistence(
                timeout: Self.webviewTimeout),
            "the nested sub-issue never opened")

        app.buttons["Back to projects"].firstMatch.tap()
        XCTAssertTrue(
            app.staticTexts["keepalive should feed liveness, not the timer"].waitForExistence(
                timeout: Self.webviewTimeout),
            "the child card did not return after its child popped")

        app.buttons["Back to projects"].firstMatch.tap()

        let title = app.staticTexts[Self.title]
        XCTAssertTrue(
            title.waitForExistence(timeout: Self.webviewTimeout),
            "the card we came back to is gone — its webview never came back")

        let shot = try XCTUnwrap(screenPixels(), "could not read the screen")
        XCTAssertGreaterThan(
            shot.inkCoverage(in: bodyBand(app)), 0.005,
            "the card came back blank — laid out, but painting nothing")
        attachScreenshot(app, name: "card-after-nested-sub-issue")
    }

    private func bodyBand(_ app: XCUIApplication) -> CGRect {
        let field = app.descendants(matching: .any)[IssueDockFields.field].firstMatch
        let top = app.buttons["issue-menu"].exists ? app.buttons["issue-menu"].frame.maxY : 120
        let bottom = field.exists ? field.frame.minY : app.frame.height - 100
        return CGRect(
            x: app.frame.minX + 20, y: top + 8,
            width: app.frame.width - 40, height: max(0, bottom - top - 16))
    }
}
