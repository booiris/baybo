import UIKit
import XCTest

/// The card page itself, headless — the webview half, which had no tier at all
/// until `-baybo-demo-card` started seeding the store (`IssueStore.seedDemoCard`).
///
/// What is worth pinning here is what the other tiers cannot see. `vitest`
/// renders this page in jsdom, which has no layout and no paint: it can assert
/// that a chip carries its value as `data-priority` and that the chips come
/// after the description in the DOM, and it is blind to whether either fact
/// survives to the screen. So this file asks the two questions jsdom cannot — where things
/// LAND, and what colour they actually are — plus the one thing that is not in
/// the webview at all: the card's run log, which now lives in the native ⋯.
final class ProjectCardUITests: BayboUITestCase {
    private static let cardArguments = [
        "-baybo-open-home", "-baybo-demo-projects", "-baybo-demo-card",
    ]
    private static let title = "the dial loop drops its subscription"
    private static let statusChip = "In Progress"
    private static let priorityChip = "Urgent"
    private static let assigneeChip = "@dev-1"

    /// The board itself, for the two cases that start from a row.
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

    /// **Title, then the text, then the state.** A card is opened to find out
    /// what it is called and what it says; the three pickers used to sit
    /// between those two, so the first screen was a title, a row of pills, a
    /// line of provenance, and then — if there was room — the first sentence.
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

    /// **The chips are actually coloured** — a claim about pixels, and the one
    /// thing every other tier here is blind to. `data-priority="urgent"` on the
    /// button proves only that the value reached the DOM; whether a hue came
    /// out the other side depends on a `color-mix()` in a stylesheet, in a
    /// WebKit that has to support it.
    ///
    /// The wash behind the word is deliberately faint, so what carries the
    /// coverage is the word itself. The assignee chip is the control: a person
    /// is not a state, it is the one chip with no hue, and a rule that leaked
    /// onto every chip would light it up too.
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

    /// **A ⋯ with nothing in it is not drawn.** Both of the button's
    /// unconditional entries left on 2026-08-26 — the description editor lost
    /// its door and Rebuild moved to the board row's long press — so what is
    /// left is about runs, and a card that has never run has nothing to say
    /// behind it.
    func testACardWithNoRunsHasNoMenu() {
        let app = openBoard()
        // The board opens on In Progress, and every card there is running by
        // construction — a card that has never run lives in Todo.
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

    /// The run log, in the ⋯ — every attempt, newest first.
    ///
    /// An attempt that never got a slot has no session and therefore no
    /// transcript. It is still a row somebody wants to SEE, so it is listed and
    /// DISABLED rather than hidden: the difference between "there was no third
    /// attempt" and "the third attempt never ran" is the whole reason to open
    /// this menu.
    func testTheRunLogLivesInTheCardsMenu() {
        let app = openCard()
        app.buttons["issue-menu"].tap()

        let runs = app.buttons["Runs"]
        XCTAssertTrue(runs.waitForExistence(timeout: 3), "the ⋯ offers no run log")
        runs.tap()

        let live = app.buttons["#3 · Working · @dev-1"]
        XCTAssertTrue(live.waitForExistence(timeout: 3), "the live run is not in the log")
        attachScreenshot(app, name: "card-runs-menu")

        // The server's sentence about WHY a run failed: DRAWN as the row's
        // subtitle, and read off its accessibility LABEL, because a menu row
        // exposes its subtitle nowhere else (see `runReading`). It was on the
        // page's run list before this menu existed, and this is the only place
        // left that can carry it.
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

    /// **A chip is a control.** All three posted `pick` and native dropped it
    /// on the floor — `picking` was written and never read — so status,
    /// priority and assignee were inert from the day they were drawn until
    /// 2026-08-26. Colouring them made that worse rather than better, which is
    /// why this file pins the press, the sheet and the card that comes back.
    ///
    /// The demo resolves the write locally (`ProjectsStore.write`'s demo
    /// branch), and the card re-seeds from the board it just edited — so what
    /// this asserts is the whole round trip minus the gateway: chip → sheet →
    /// board write → the card page saying something new.
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

    /// **The same control is the same width on both surfaces.** The card's
    /// pill sat at a gutter of its own while the chat's held a narrower
    /// resting width and stretched on focus, so pushing a card off a
    /// conversation changed the shape of the thing you type into. Measured
    /// across two launches rather than asserted against a number, because the
    /// requirement is that they AGREE — a number here would go stale the day
    /// the chat's changes and would not notice.
    func testTheFieldMatchesTheChatsRestingWidth() {
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
    }

    /// And it stretches on the same beat. At rest the pill holds a moderate
    /// width; focus takes it out toward the edges with the keyboard.
    func testFocusingTheFieldWidensThePill() {
        let app = openCard()
        let field = app.descendants(matching: .any)[IssueDockFields.field].firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 10), "no card composer")
        let resting = field.frame.width

        field.tap()
        XCTAssertTrue(
            app.keyboards.firstMatch.waitForExistence(timeout: 5), "the field never focused")
        // 40pt gutters at rest, 14 focused — the pill gains 52 in all.
        XCTAssertEqual(
            field.frame.width, resting + 52, accuracy: 1,
            "focusing the card's field did not stretch the pill the way the chat's does")
    }

    /// **No prompt inside the pill.** What a comment will do is already said on
    /// the hint line above it; the grey sentence in the field was a third voice
    /// saying the obvious. The words stay as the field's accessibility name,
    /// which is the one thing a placeholder was still buying.
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

    /// **The way back down.** A card is opened at the top and its Activity is
    /// at the bottom; until 2026-08-26 the only way back was to drag. The disc
    /// is the chat's, and it appears on the same rule: only when the newest
    /// thing is off screen.
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

    /// The page can be scrolled clear of the dock.
    ///
    /// The webview is full-bleed UNDER a floating dock and pads itself by the
    /// dock's height, which native measures off the dock's own geometry and
    /// streams over the bridge — nothing here is laid out to fit. So the
    /// clearance is the product of two numbers that no other tier holds
    /// together: the inset native computes (`screenHeight - dock.minY`) and the
    /// padding the page applies. Either one wrong by the dock's height, and the
    /// last comment on every card is unreadable while the page is at the
    /// bottom, which is exactly where a card is read from.
    func testTheLastCommentCanBeScrolledClearOfTheDock() {
        let app = openCard()
        let last = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'once the fence lands'")
        ).firstMatch
        XCTAssertTrue(
            last.waitForExistence(timeout: Self.webviewTimeout),
            "the card's last comment never rendered")

        // Swipe until the scroller stops moving: the page is short, so this is
        // two or three, and a fixed count would be a bet on the fixture's
        // height rather than on the padding under test.
        let started = last.frame.minY
        var previous = CGFloat.greatestFiniteMagnitude
        for _ in 0..<8 {
            let y = last.frame.minY
            if abs(y - previous) < 1 { break }
            previous = y
            app.swipeUp()
        }
        // …and the fixture really is taller than the screen. Without this the
        // whole case passes on a page that never scrolled at all, which is the
        // one state in which its clearance proves nothing.
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

    /// **The page scrolls in ONE axis.** `overflow-y: auto` alone computes the
    /// other axis to `auto` as well, so one thing wider than the reading band —
    /// the fixture's blocked note carries an identifier with no break
    /// opportunity in it, which is how this arrives in real life — panned the
    /// whole card sideways under the finger, native header and all.
    ///
    /// A horizontal drag must move nothing. Asserted on the TITLE rather than
    /// on the dragged element: a pan moves the whole scroller, and the title is
    /// the one thing on the page whose left edge is the reading margin itself.
    ///
    /// It pins the OUTCOME, not either of the two rules that produce it —
    /// `overflow-x: hidden` and the page-wide `overflow-wrap: anywhere` — since
    /// each alone is enough to hold this fixture still. Verified by removing
    /// both: the title's left edge goes 20 → 0 and this fails.
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
}
