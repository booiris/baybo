import XCTest

final class ProjectsUITests: BayboUITestCase {
    private static let demoCardTitle = "the dial loop drops its subscription"

    private func openProjects() -> XCUIApplication {
        launch(["-baybo-open-home", "-baybo-home-tab", "projects", "-baybo-demo-projects"])
    }

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

    func testTheListReordersByWhatWasOpenedLastAndRemembersIt() {
        let app = openProjects()
        let cards = app.buttons.matching(identifier: "project-card")
        XCTAssertTrue(cards.element(boundBy: 0).waitForExistence(timeout: 5))
        XCTAssertTrue(cards.element(boundBy: 0).label.contains("rglide"), "unexpected seed order")

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

    func testTheBoardShowsOneStageAtATimeAndSwitchesOnASegment() {
        let app = openBoard()
        let inProgress = app.buttons["stage-in-progress"]
        XCTAssertTrue(inProgress.waitForExistence(timeout: 5), "the board never painted")
        XCTAssertTrue(app.buttons["issue-row-41"].waitForExistence(timeout: 3))
        XCTAssertFalse(app.buttons["issue-row-43"].exists)

        app.buttons["stage-todo"].tap()
        XCTAssertTrue(
            app.buttons["issue-row-43"].waitForExistence(timeout: 3),
            "switching stage did not swap the cards")
        XCTAssertFalse(app.buttons["issue-row-41"].exists)
    }

    func testTheStageBarStaysPutWhileTheBoardScrolls() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["stage-todo"].waitForExistence(timeout: 5))
        app.buttons["stage-todo"].tap()

        let bar = app.buttons["stage-todo"]
        XCTAssertTrue(app.buttons["issue-row-43"].waitForExistence(timeout: 3))
        let below = app.buttons["issue-row-51"]
        XCTAssertFalse(below.exists, "the fixture's Todo fits on screen — nothing here scrolls")
        let before = bar.frame

        app.swipeUp()

        XCTAssertTrue(below.waitForExistence(timeout: 3), "the board did not scroll")
        XCTAssertEqual(
            bar.frame.minY, before.minY, accuracy: 0.5,
            "the stage bar scrolled away with the board")
        XCTAssertTrue(bar.isHittable, "and it is still pressable where it sits")
    }

    func testEveryPartOfTheRowOpensTheCardNotJustItsText() {
        let dead: [(String, CGVector)] = [
            ("the right margin", CGVector(dx: 0.98, dy: 0.5)),
            ("the padding above the text", CGVector(dx: 0.5, dy: 0.03)),
            ("the padding below it", CGVector(dx: 0.5, dy: 0.97)),
        ]
        for (where_, offset) in dead {
            let app = openBoard()
            let row = app.buttons["issue-row-41"]
            XCTAssertTrue(row.waitForExistence(timeout: 8))
            row.coordinate(withNormalizedOffset: offset).tap()
            XCTAssertTrue(
                app.staticTexts[Self.demoCardTitle].waitForExistence(
                    timeout: BayboUITestCase.webviewTimeout),
                "tapping \(where_) should open the card")
            app.terminate()
        }
    }

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
        XCTAssertFalse(app.buttons["move-in-progress"].isEnabled)
    }

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

    func testOnlyAMoveThatStartedNothingOffersUndo() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["issue-row-41"].waitForExistence(timeout: 5))

        app.buttons["issue-row-41"].press(forDuration: 1.0)
        app.buttons["Move"].firstMatch.tap()
        XCTAssertTrue(app.buttons["move-review"].waitForExistence(timeout: 3))
        app.buttons["move-review"].tap()
        XCTAssertTrue(
            app.buttons["board-undo"].waitForExistence(timeout: 4),
            "a move that started nothing should be undoable")

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
        XCTAssertFalse(
            app.buttons["board-undo"].waitForExistence(timeout: 3),
            "a move that queued a run must not offer to undo it")
    }

    func testTheWaitingStripsAnswerButtonsDoNotOpenTheCard() {
        let app = openBoard()
        let deny = app.buttons["waiting-deny-41"]
        XCTAssertTrue(deny.waitForExistence(timeout: 5), "no approval row in the strip")
        deny.tap()
        XCTAssertTrue(
            app.buttons["stage-in-progress"].waitForExistence(timeout: 3),
            "answering must not navigate away from the board")
    }

    func testOnlyParkedApprovalsReachTheStrip() {
        let app = openBoard()
        XCTAssertTrue(app.otherElements["waiting-strip"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["waiting-approve-41"].exists, "no approval row")

        XCTAssertEqual(
            app.descendants(matching: .any).matching(
                NSPredicate(format: "identifier BEGINSWITH 'waiting-row-'")
            ).count, 1,
            "the strip should hold the one parked prompt and nothing else")

        app.buttons["stage-todo"].tap()
        app.buttons["stage-in-progress"].tap()
        XCTAssertTrue(
            app.buttons["issue-row-42"].label.contains("Run failed"),
            "a failed run must still be visible on the card")
    }

    func testTheBoardMenuOpensTheScreensBehindIt() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["board-menu"].waitForExistence(timeout: 5))

        app.buttons["board-menu"].tap()
        XCTAssertTrue(app.buttons["Team"].waitForExistence(timeout: 3), "no Team entry")
        app.buttons["Team"].tap()
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

    func testAutoMergeReadsTheBoardAndSaysWhatEachStateMeans() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["board-budget-chip"].waitForExistence(timeout: 5))
        app.buttons["board-budget-chip"].tap()

        let toggle = app.switches["settings-auto-merge"]
        XCTAssertTrue(toggle.waitForExistence(timeout: 5), "settings never presented")
        XCTAssertEqual(
            toggle.value as? String, "1",
            "this board merges — a switch that starts off is not reading the board")
        let merges = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'assignee merges the branch'"))
        XCTAssertTrue(merges.element.exists, "the on state must say who merges, and into what")

        toggle.tap()
        let handsOver = app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS 'hands its branch over'"))
        XCTAssertTrue(
            handsOver.element.waitForExistence(timeout: 3),
            "the off state must say the branch is handed over, not lost")
        XCTAssertFalse(merges.element.exists, "both sentences must never be on screen at once")
    }

    func testArchivingAsksFirstAndTheTriggerSurvivesADismissal() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["board-budget-chip"].waitForExistence(timeout: 5))
        app.buttons["board-budget-chip"].tap()

        let archive = app.buttons["settings-archive"]
        XCTAssertTrue(archive.waitForExistence(timeout: 5), "settings never presented")
        archive.tap()
        let confirm = app.staticTexts["Archive this project?"]
        XCTAssertTrue(confirm.waitForExistence(timeout: 3), "the pill did not raise the confirm")

        app.coordinate(withNormalizedOffset: CGVector(dx: 0.06, dy: 0.5)).tap()
        sleep(1)
        XCTAssertFalse(confirm.exists, "the scrim did not dismiss it")

        archive.coordinate(withNormalizedOffset: CGVector(dx: 0.06, dy: 0.5)).tap()
        XCTAssertTrue(
            confirm.waitForExistence(timeout: 3),
            "the pill's interior is dead, or the trigger did not survive a scrim dismiss")
    }

    func testTheFilterWidensToCancelledAndNarrowsToRunning() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["board-filter-chip"].waitForExistence(timeout: 5))
        app.buttons["board-filter-chip"].tap()

        let running = app.buttons["filter-running"]
        XCTAssertTrue(running.waitForExistence(timeout: 3), "the filter sheet never presented")
        XCTAssertEqual(running.value as? String, "0")
        running.tap()
        XCTAssertEqual(running.value as? String, "1")

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

    func testFilingACardOpensInTheStageYouWereOnAndRefusesAnUnstaffedRun() {
        let app = openBoard()
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

        app.buttons["new-issue-stage-in-progress"].tap()
        XCTAssertFalse(
            create.isEnabled, "In Progress with nobody on it must not be filable")
        let note = app.staticTexts["new-issue-consequence"]
        XCTAssertTrue(note.waitForExistence(timeout: 3), "no consequence line")
        XCTAssertTrue(
            note.label.contains("Needs an assignee"),
            "the line should say what is missing; got \(note.label)")

        app.buttons["new-issue-stage-todo"].tap()
        XCTAssertTrue(create.isEnabled)
    }

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

    func testACardCanBeRebuiltFromTheList() {
        let app = openBoard()
        let row = app.buttons["issue-row-41"]
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.press(forDuration: 1.0)

        let rebuild = app.buttons["Rebuild this card"].firstMatch
        XCTAssertTrue(rebuild.waitForExistence(timeout: 3), "no rebuild entry on the long press")
        attachScreenshot(app, name: "row-long-press")
        rebuild.tap()

        XCTAssertTrue(
            app.staticTexts["#41 rebuilds the next time it opens"].waitForExistence(timeout: 3),
            "the rebuild said nothing, so nothing happened as far as the operator can see")
        attachScreenshot(app, name: "rebuild-toast")

        row.tap()
        XCTAssertTrue(
            app.staticTexts[Self.demoCardTitle].waitForExistence(
                timeout: BayboUITestCase.webviewTimeout),
            "the card never came back after its copy was thrown away")
    }

    func testACardCanBeCancelledAndReopenedFromTheList() {
        let app = openBoard()
        let row = app.buttons["issue-row-41"]
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.press(forDuration: 1.0)

        let cancel = app.buttons["Cancel issue"].firstMatch
        XCTAssertTrue(cancel.waitForExistence(timeout: 3), "no Cancel issue on the row")
        cancel.tap()

        XCTAssertTrue(
            app.staticTexts["Cancel this issue?"].waitForExistence(timeout: 3),
            "Cancel skipped its destructive confirmation")
        XCTAssertTrue(
            app.staticTexts.containing(
                NSPredicate(format: "label CONTAINS 'keeps its number'")
            ).firstMatch.exists,
            "the confirmation does not explain that the row and history survive")
        app.buttons["Cancel issue"].tap()

        XCTAssertTrue(
            row.waitForNonExistence(timeout: 3),
            "a cancelled card should leave the live-work list immediately")
        app.buttons["board-filter-chip"].tap()
        let showCancelled = app.buttons["filter-cancelled"]
        XCTAssertTrue(showCancelled.waitForExistence(timeout: 3))
        showCancelled.tap()
        let filterTitle = app.staticTexts["Filter"].firstMatch
        XCTAssertTrue(filterTitle.exists)
        filterTitle.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
            .press(
                forDuration: 0.05,
                thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.95)))
        XCTAssertTrue(
            showCancelled.waitForNonExistence(timeout: 3),
            "the filter sheet did not dismiss")

        XCTAssertTrue(
            row.waitForExistence(timeout: 3),
            "Show cancelled did not make the recoverable row reachable")
        row.press(forDuration: 1.0)
        let reopen = app.buttons["Reopen issue"].firstMatch
        XCTAssertTrue(reopen.waitForExistence(timeout: 3), "a cancelled row cannot be reopened")
        reopen.tap()

        XCTAssertTrue(row.waitForExistence(timeout: 3))
        row.press(forDuration: 1.0)
        XCTAssertTrue(
            app.buttons["Cancel issue"].firstMatch.waitForExistence(timeout: 3),
            "reopening did not restore the row's live action")
    }

    func testAnsweringAWaitingRowRetiresItImmediately() {
        let app = openBoard()
        let approve = app.buttons["waiting-approve-41"]
        XCTAssertTrue(approve.waitForExistence(timeout: 5), "no approval row")
        approve.tap()
        XCTAssertFalse(
            app.buttons["waiting-approve-41"].waitForExistence(timeout: 4),
            "an answered approval must leave the strip on the press")

    }

    func testTheStripDisappearsOnceTheLastPromptIsAnswered() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["waiting-approve-41"].waitForExistence(timeout: 5))
        app.buttons["waiting-approve-41"].tap()
        XCTAssertFalse(
            app.otherElements["waiting-strip"].waitForExistence(timeout: 4),
            "an empty strip must not linger as a header over nothing")
    }

    func testTheNewBoardFormRefusesToCreateUntilItIsNamed() {
        let app = openProjects()
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
