import XCTest

/// The Projects tab's cards root, over `-baybo-demo-projects`.
///
/// The three claims worth a headless smoke are the three the design turns on:
/// the tab's badge (this app's first `.badge`), a card press pushing the board
/// rather than switching a tab, and the new-board form being reachable from
/// both the cards list and the empty state.
final class ProjectsUITests: BayboUITestCase {
    private func openProjects() -> XCUIApplication {
        launch(["-baybo-open-home", "-baybo-home-tab", "projects", "-baybo-demo-projects"])
    }

    /// One card per board, archived ones withheld until asked for.
    func testTheCardsRootListsLiveBoardsAndHidesArchivedOnesBehindAToggle() {
        let app = openProjects()
        let cards = app.buttons.matching(identifier: "project-card")
        XCTAssertTrue(
            cards.element(boundBy: 0).waitForExistence(timeout: 5),
            "the cards root never painted")
        XCTAssertEqual(cards.count, 3, "three live boards, the archived one withheld")

        let toggle = app.buttons["Show archived (1)"]
        XCTAssertTrue(toggle.waitForExistence(timeout: 3), "no archived toggle")
        toggle.tap()
        XCTAssertTrue(
            app.buttons.matching(identifier: "project-card").count == 4,
            "the archived board did not join the list")
    }

    /// The Projects tab carries a badge for what the boards are waiting on.
    ///
    /// Asserted in PIXELS, and that is not belt-and-braces: this is the repo's
    /// first `.badge`, and it turns out SwiftUI exposes it to accessibility
    /// nowhere at all — the tab item's label stays the bare `Projects`, and the
    /// badge has no child element (dumped from the live tree). So the drawn
    /// disc is the only evidence the badge exists, and a test reading `label`
    /// would have passed a build that rendered no badge whatsoever.
    ///
    /// Deck is the control. Without it "there is red near the tab bar" would be
    /// satisfied by any red anywhere in the strip, including the neighbouring
    /// Chats badge.
    func testTheProjectsTabCarriesABadgeAndAQuietTabDoesNot() {
        let app = openProjects()
        XCTAssertTrue(
            app.buttons.matching(identifier: "project-card").element(boundBy: 0)
                .waitForExistence(timeout: 5))

        let projects = app.tabBars.buttons["square.stack.3d.up"]
        let deck = app.tabBars.buttons["rectangle.stack"]
        XCTAssertTrue(projects.waitForExistence(timeout: 3), "no Projects tab item")
        XCTAssertTrue(deck.exists, "no Deck tab item")

        guard let pixels = screenPixels() else {
            return XCTFail("no screenshot")
        }
        let onProjects = pixels.redCoverage(in: projects.frame)
        let onDeck = pixels.redCoverage(in: deck.frame)
        XCTAssertGreaterThan(
            onProjects, 0.01,
            "the Projects tab should carry a badge disc; red coverage was \(onProjects)")
        XCTAssertLessThan(
            onDeck, 0.001,
            "Deck has nothing waiting and must carry no badge; red coverage was \(onDeck)")
    }

    /// The list is ordered by what this phone opened last, and the order
    /// survives a relaunch — which is the only part of it a user can see go
    /// wrong.
    func testTheListReordersByWhatWasOpenedLastAndRemembersIt() {
        let app = openProjects()
        let cards = app.buttons.matching(identifier: "project-card")
        XCTAssertTrue(cards.element(boundBy: 0).waitForExistence(timeout: 5))
        // The fixture's server order: rglide, atlas, scratch.
        XCTAssertTrue(cards.element(boundBy: 0).label.contains("rglide"), "unexpected seed order")

        // Open the THIRD board, then come back.
        cards.element(boundBy: 2).tap()
        let back = app.buttons["Back to projects"]
        XCTAssertTrue(back.waitForExistence(timeout: 5))
        back.tap()

        XCTAssertTrue(
            app.buttons.matching(identifier: "project-card").element(boundBy: 0)
                .waitForExistence(timeout: 5))
        XCTAssertTrue(
            app.buttons.matching(identifier: "project-card").element(boundBy: 0).label
                .contains("scratch"),
            "the board just opened should lead the list")

        // Relaunch WITHOUT resetting the store: the stamps are on disk, and
        // outliving the process is the whole point of writing them there.
        app.terminate()
        let relaunched = XCUIApplication()
        relaunched.launchArguments = [
            "-baybo-open-home", "-baybo-home-tab", "projects", "-baybo-demo-projects",
            "-baybo.lang", "en", "-AppleLanguages", "(en)", "-AppleLocale", "en_US",
        ]
        relaunched.launch()
        let first = relaunched.buttons.matching(identifier: "project-card").element(boundBy: 0)
        XCTAssertTrue(first.waitForExistence(timeout: 8))
        XCTAssertTrue(
            first.label.contains("scratch"),
            "the order must survive a relaunch; got \(first.label)")
        relaunched.terminate()
    }

    /// A press opens the board as a PUSH — the shell's whole point is that the
    /// board covers the tab bar, so a tab bar still on screen means the card
    /// merely switched sections.
    func testPressingACardPushesTheBoardOverTheTabBar() {
        let app = openProjects()
        let card = app.buttons.matching(identifier: "project-card").element(boundBy: 0)
        XCTAssertTrue(card.waitForExistence(timeout: 5))
        card.tap()

        let back = app.buttons["Back to projects"]
        XCTAssertTrue(back.waitForExistence(timeout: 5), "the board never pushed")
        XCTAssertFalse(
            app.tabBars.firstMatch.isHittable,
            "a pushed board must cover the tab bar, not sit beside it")

        back.tap()
        XCTAssertTrue(
            app.buttons.matching(identifier: "project-card").element(boundBy: 0)
                .waitForExistence(timeout: 5),
            "backing out did not return to the cards root")
    }

    private func openBoard() -> XCUIApplication {
        launch(["-baybo-open-home", "-baybo-demo-projects", "-baybo-demo-board"])
    }

    /// The stage wall: one stage at a time, and the segment carries its live
    /// count.
    func testTheBoardShowsOneStageAtATimeAndSwitchesOnASegment() {
        let app = openBoard()
        let inProgress = app.buttons["stage-in-progress"]
        XCTAssertTrue(inProgress.waitForExistence(timeout: 5), "the board never painted")
        XCTAssertTrue(app.buttons["issue-row-41"].waitForExistence(timeout: 3))
        // Todo's cards are not on screen while In Progress is the stage.
        XCTAssertFalse(app.buttons["issue-row-43"].exists)

        app.buttons["stage-todo"].tap()
        XCTAssertTrue(
            app.buttons["issue-row-43"].waitForExistence(timeout: 3),
            "switching stage did not swap the cards")
        XCTAssertFalse(app.buttons["issue-row-41"].exists)
    }

    /// **The consequence is the sheet.** These two sentences are the reason it
    /// exists, and both are things a desktop board never has to say: a move out
    /// of In Progress does not stop the run, and a move in starts one.
    func testTheMoveSheetSaysWhatEachMoveWillDo() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["issue-row-41"].waitForExistence(timeout: 5))
        app.buttons["issue-row-41"].press(forDuration: 1.0)

        let move = app.buttons["Move"].firstMatch
        XCTAssertTrue(move.waitForExistence(timeout: 3), "no Move in the long-press menu")
        move.tap()

        let todo = app.buttons["move-todo"]
        XCTAssertTrue(todo.waitForExistence(timeout: 3), "the Move sheet never presented")
        XCTAssertTrue(
            todo.label.contains("only Stop ends it"),
            "moving out of In Progress must say the run keeps going; got \(todo.label)")

        let done = app.buttons["move-done"]
        XCTAssertTrue(
            done.label.contains("worktree"),
            "Done must say what it reclaims; got \(done.label)")
        // The card is already In Progress, so that row is the current one.
        XCTAssertFalse(app.buttons["move-in-progress"].isEnabled)
    }

    /// A card with nobody on it cannot be started, and the row says so rather
    /// than sitting disabled — tapping it opens the picker and finishes the
    /// move afterwards.
    func testAnUnassignedCardOffersThePickerInsteadOfADeadRow() {
        let app = openBoard()
        app.buttons["stage-todo"].tap()
        let row = app.buttons["issue-row-44"]
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.press(forDuration: 1.0)
        app.buttons["Move"].firstMatch.tap()

        let inProgress = app.buttons["move-in-progress"]
        XCTAssertTrue(inProgress.waitForExistence(timeout: 3))
        XCTAssertTrue(
            inProgress.label.contains("Needs an assignee"),
            "an unassigned card must say what is missing; got \(inProgress.label)")
        XCTAssertTrue(inProgress.isEnabled, "the row must not be dead")
        inProgress.tap()
        XCTAssertTrue(
            app.buttons["assign-dev-1"].waitForExistence(timeout: 3),
            "tapping it should open the assignee picker")
    }

    /// A move that started nothing can be taken back; a move that STARTED a
    /// run cannot, and must not offer to.
    ///
    /// Undoing a move into In Progress would put the card back while the run it
    /// triggered kept going — so the toast would be offering to unwind
    /// something it cannot reach, and the operator only finds that out after
    /// pressing. Both halves are asserted: the first build shipped one toast
    /// view that always drew Undo.
    func testOnlyAMoveThatStartedNothingOffersUndo() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["issue-row-41"].waitForExistence(timeout: 5))

        // Out of In Progress: starts nothing, so it is reversible.
        app.buttons["issue-row-41"].press(forDuration: 1.0)
        app.buttons["Move"].firstMatch.tap()
        XCTAssertTrue(app.buttons["move-review"].waitForExistence(timeout: 3))
        app.buttons["move-review"].tap()
        XCTAssertTrue(
            app.buttons["board-undo"].waitForExistence(timeout: 4),
            "a move that started nothing should be undoable")

        // Back into In Progress: this one queues a run.
        app.buttons["stage-review"].tap()
        XCTAssertTrue(app.buttons["issue-row-41"].waitForExistence(timeout: 4))
        app.buttons["issue-row-41"].press(forDuration: 1.0)
        app.buttons["Move"].firstMatch.tap()
        let inProgress = app.buttons["move-in-progress"]
        XCTAssertTrue(inProgress.waitForExistence(timeout: 3))
        XCTAssertTrue(
            inProgress.label.contains("Starts a run"),
            "the row should say it starts one; got \(inProgress.label)")
        inProgress.tap()
        // Give the toast the same window the undoable one got.
        XCTAssertFalse(
            app.buttons["board-undo"].waitForExistence(timeout: 3),
            "a move that queued a run must not offer to undo it")
    }

    /// The Waiting strip's answer buttons must not fall through to the row.
    ///
    /// The text column opens the card; the buttons answer. A default-styled
    /// button inside a tappable row hands its press to the row, which would
    /// make Approve navigate instead of approving — silently, and only on the
    /// one control in this app where a mis-tap runs a command.
    func testTheWaitingStripsAnswerButtonsDoNotOpenTheCard() {
        let app = openBoard()
        let deny = app.buttons["waiting-deny-41"]
        XCTAssertTrue(deny.waitForExistence(timeout: 5), "no approval row in the strip")
        deny.tap()
        // Still on the board: the board's own segments are the proof, since a
        // pushed card screen covers them.
        XCTAssertTrue(
            app.buttons["stage-in-progress"].waitForExistence(timeout: 3),
            "answering must not navigate away from the board")
    }

    /// All four kinds, and each card at most once — a card already waiting for
    /// something answerable does not also queue as news.
    func testTheWaitingStripCarriesEveryKindAndNoCardTwice() {
        let app = openBoard()
        XCTAssertTrue(app.otherElements["waiting-strip"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["waiting-approve-41"].exists, "no approval row")
        XCTAssertTrue(app.buttons["waiting-retry-42"].exists, "no failed row")
        XCTAssertTrue(app.buttons["waiting-answer-38"].exists, "no question row")
        // #41 carries unread AND an approval; only the approval may be listed.
        XCTAssertEqual(
            app.descendants(matching: .any).matching(identifier: "waiting-row-41").count, 1,
            "a card must not appear in the strip twice")
    }

    /// The board's ⋯ opens the four screens P7 added.
    ///
    /// A menu is exactly the kind of wiring that compiles, looks right in
    /// review, and opens nothing: a sheet bound to the wrong flag, or one whose
    /// `if let` never resolves, fails silently.
    func testTheBoardMenuOpensTheScreensBehindIt() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["board-menu"].waitForExistence(timeout: 5))

        app.buttons["board-menu"].tap()
        XCTAssertTrue(app.buttons["Team"].waitForExistence(timeout: 3), "no Team entry")
        app.buttons["Team"].tap()
        // The demo has no gateway, so the sheet shows its read-failed line
        // rather than rows — which is still the sheet, presented.
        XCTAssertTrue(
            app.staticTexts["Team"].waitForExistence(timeout: 5), "the team sheet never presented")
        app.buttons["Done"].firstMatch.tap()

        XCTAssertTrue(app.buttons["board-menu"].waitForExistence(timeout: 3))
        app.buttons["board-menu"].tap()
        XCTAssertTrue(app.buttons["Activity"].waitForExistence(timeout: 3), "no Activity entry")
        app.buttons["Activity"].tap()
        XCTAssertTrue(
            app.staticTexts["Activity"].waitForExistence(timeout: 5),
            "the activity sheet never presented")
    }

    /// A cancelled card is hidden by default and still reachable — dropping it
    /// from the client entirely is how a card somebody wants to reopen becomes
    /// unreachable from the phone.
    func testTheFilterWidensToCancelledAndNarrowsToRunning() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["board-filter-chip"].waitForExistence(timeout: 5))
        app.buttons["board-filter-chip"].tap()

        let running = app.buttons["filter-running"]
        XCTAssertTrue(running.waitForExistence(timeout: 3), "the filter sheet never presented")
        XCTAssertEqual(running.value as? String, "0")
        running.tap()
        XCTAssertEqual(running.value as? String, "1")

        // Showing cancelled WIDENS the board, so it must not count as a filter —
        // otherwise an un-narrowed list wears a filter mark.
        let cancelled = app.buttons["filter-cancelled"]
        XCTAssertTrue(cancelled.exists)
        cancelled.tap()
        XCTAssertEqual(cancelled.value as? String, "1")

        XCTAssertTrue(app.buttons["filter-clear"].exists, "Clear should appear once narrowed")
        app.buttons["filter-clear"].tap()
        XCTAssertEqual(running.value as? String, "0", "Clear drops the narrowings")
        XCTAssertEqual(
            cancelled.value as? String, "1",
            "Clear must leave the widening alone — it is not a narrowing")
    }

    /// Filing a card: the board's + opens the form in the column you were on,
    /// and In Progress without somebody on it is refused before it is sent.
    ///
    /// That refusal is the server's (`validate_staffing`), and offering a
    /// button that can only 400 is worse than not offering it.
    func testFilingACardOpensInTheStageYouWereOnAndRefusesAnUnstaffedRun() {
        let app = openBoard()
        // Land on Todo first, so "the column you were on" is testable at all.
        XCTAssertTrue(app.buttons["stage-todo"].waitForExistence(timeout: 5))
        app.buttons["stage-todo"].tap()

        let plus = app.buttons["board-new-issue"]
        XCTAssertTrue(plus.waitForExistence(timeout: 3), "no way to file a card")
        plus.tap()

        let todoChip = app.buttons["new-issue-stage-todo"]
        XCTAssertTrue(todoChip.waitForExistence(timeout: 5), "the form never pushed")
        XCTAssertEqual(
            todoChip.value as? String, "1", "the form should open in the column you were on")

        let create = app.buttons["new-issue-create"]
        XCTAssertFalse(create.isEnabled, "an untitled card must not be filable")

        let title = app.textFields["new-issue-title"]
        XCTAssertTrue(title.exists)
        title.tap()
        title.typeText("the relay token format")
        XCTAssertTrue(create.isEnabled, "a titled card in Todo is filable")

        // In Progress with nobody on it: the board refuses it, so the form does.
        app.buttons["new-issue-stage-in-progress"].tap()
        XCTAssertFalse(
            create.isEnabled, "In Progress with nobody on it must not be filable")
        let note = app.staticTexts["new-issue-consequence"]
        XCTAssertTrue(note.waitForExistence(timeout: 3), "no consequence line")
        XCTAssertTrue(
            note.label.contains("Needs an assignee"),
            "the line should say what is missing; got \(note.label)")

        // Back to Todo and it is filable again — the refusal is about the
        // column, not about the card.
        app.buttons["new-issue-stage-todo"].tap()
        XCTAssertTrue(create.isEnabled)
    }

    /// An archived board takes no writes, so the slot that would file a card
    /// carries the chip explaining why instead of a button that can only fail.
    func testAnArchivedBoardOffersNoWayToFileACard() {
        let app = openProjects()
        XCTAssertTrue(app.buttons["Show archived (1)"].waitForExistence(timeout: 5))
        app.buttons["Show archived (1)"].tap()

        let cards = app.buttons.matching(identifier: "project-card")
        XCTAssertTrue(cards.element(boundBy: 3).waitForExistence(timeout: 3))
        cards.element(boundBy: 3).tap()

        XCTAssertTrue(app.buttons["Back to projects"].waitForExistence(timeout: 5))
        XCTAssertFalse(
            app.buttons["board-new-issue"].exists,
            "an archived board must not offer to file a card")
    }

    /// The new-board form is a pushed route, not a sheet — deliberately, so the
    /// name field can rise with the keyboard (the home shell opts out of
    /// keyboard avoidance wholesale). Create stays disabled until it is named.
    func testTheNewBoardFormRefusesToCreateUntilItIsNamed() {
        let app = openProjects()
        // The header's + — the dashed card at the foot of the list is gone,
        // because it put the one thing you cannot reach any other way behind
        // however many boards you happen to have.
        let new = app.buttons["header-action"]
        XCTAssertTrue(new.waitForExistence(timeout: 5), "no new-board affordance in the header")
        new.tap()

        let name = app.textFields["new-project-name"]
        XCTAssertTrue(name.waitForExistence(timeout: 5), "the new-board form never pushed")
        let create = app.buttons["new-project-create"]
        XCTAssertTrue(create.exists)
        XCTAssertFalse(create.isEnabled, "an unnamed board must not be creatable")

        name.tap()
        name.typeText("a-new-board")
        XCTAssertTrue(create.isEnabled, "a named board should be creatable")
    }
}
