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
    ///
    /// What it counts is PARKED APPROVALS, not the server's wider `/attention`
    /// — the same set a board's Waiting strip shows, so the number you press
    /// and the rows you land on agree.
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

    /// The five stages are navigation, not content: they stay put while the
    /// board scrolls under them.
    ///
    /// The trap this test is written around is that it passes trivially if
    /// the list never moves — a stage that fits on screen swipes to nothing,
    /// and "the bar did not move" would then be true of a bar that scrolls.
    /// So the scroll is PROVEN first, by a card that is below the fold before
    /// the swipe and reachable after it.
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

    /// The WHOLE row opens the card, not just the letters in it.
    ///
    /// Under `.buttonStyle(.plain)` a label's hit region is whatever it
    /// PAINTS — a `Text` hit-tests its own box and nothing else — so a row
    /// without a `contentShape` is dead wherever there is no ink.
    ///
    /// **These three points are measured, not guessed.** A probe over the row
    /// found exactly which offsets died before the fix: the far right edge and
    /// both vertical paddings. A centre tap — what `.tap()` does — landed on
    /// the title and passed either way, which is why this walks coordinates
    /// instead.
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
                app.buttons["issue-menu"].waitForExistence(timeout: 8),
                "tapping \(where_) should open the card")
            app.terminate()
        }
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

    /// **Only parked approvals reach the strip.** The fixture's board also has
    /// a failed run (#42), an agent's question (#38) and two unread cards —
    /// none of which is waiting on an answer, and each of which says itself on
    /// its own card row instead.
    func testOnlyParkedApprovalsReachTheStrip() {
        let app = openBoard()
        XCTAssertTrue(app.otherElements["waiting-strip"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["waiting-approve-41"].exists, "no approval row")

        XCTAssertEqual(
            app.descendants(matching: .any).matching(
                NSPredicate(format: "identifier BEGINSWITH 'waiting-row-'")
            ).count, 1,
            "the strip should hold the one parked prompt and nothing else")

        // The failed card still says so, on its own row.
        app.buttons["stage-todo"].tap()
        app.buttons["stage-in-progress"].tap()
        XCTAssertTrue(
            app.buttons["issue-row-42"].label.contains("Run failed"),
            "a failed run must still be visible on the card")
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

    /// Auto-merge reads the BOARD, and says which of the two things happens.
    ///
    /// The fixture's open board merges and its neighbours do not, deliberately:
    /// a switch wired to a constant `false` — or to a settings body that never
    /// carried the field, which is what this app shipped until now — looks
    /// exactly like a correct one on a board that does not merge. Starting ON
    /// is the only observation that separates them.
    ///
    /// The hint is asserted in both directions because the label alone does not
    /// say what "off" costs: whether the branch waits for you or is thrown away
    /// is the whole decision, and it lives in that sentence.
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

    /// Archiving a board: the pill, its confirm, and the way back out.
    ///
    /// The confirm this raises offers **no visible Cancel** — the system draws
    /// it as a floating card with the one action on it — so the scrim is the
    /// only way out. And a scrim dismiss is precisely what left
    /// `.confirmationDialog`'s `isPresented` latched true in the TabView shell,
    /// deadening the trigger: the bug the hand-rolled `ConfirmDialog` was
    /// written to escape. Presented from inside a SHEET it does not reproduce,
    /// and this is what says so — for a control whose only escape hatch is the
    /// dismissal that used to break it.
    ///
    /// The second press deliberately lands **off the label**, near the pill's
    /// leading edge. `OutlinePillButtonStyle` paints a 1px capsule and nothing
    /// else, so without its `contentShape` only the glyphs hit-test and the
    /// interior is dead — and a centre `.tap()`, which is all `.tap()` ever
    /// does, lands on the glyphs and passes either way. That is exactly how the
    /// logout pill shipped broken.
    ///
    /// The budget chip rather than the ⋯ entry, because the menu's "Settings"
    /// label collides with the tab bar's.
    func testArchivingAsksFirstAndTheTriggerSurvivesADismissal() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["board-budget-chip"].waitForExistence(timeout: 5))
        app.buttons["board-budget-chip"].tap()

        let archive = app.buttons["settings-archive"]
        XCTAssertTrue(archive.waitForExistence(timeout: 5), "settings never presented")
        archive.tap()
        let confirm = app.staticTexts["Archive this project?"]
        XCTAssertTrue(confirm.waitForExistence(timeout: 3), "the pill did not raise the confirm")

        // Beside the dialog card but inside the sheet: the scrim.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.06, dy: 0.5)).tap()
        sleep(1)
        XCTAssertFalse(confirm.exists, "the scrim did not dismiss it")

        archive.coordinate(withNormalizedOffset: CGVector(dx: 0.06, dy: 0.5)).tap()
        XCTAssertTrue(
            confirm.waitForExistence(timeout: 3),
            "the pill's interior is dead, or the trigger did not survive a scrim dismiss")
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

    /// The card's escape hatch is in its ⋯, and the page comes back rather
    /// than staying blank — a rebuild that reloaded the document and never
    /// re-delivered would look exactly like a hang.
    func testACardCanBeRebuiltFromItsMenu() {
        let app = launch(["-baybo-open-home", "-baybo-demo-projects", "-baybo-demo-card"])
        XCTAssertTrue(app.buttons["issue-menu"].waitForExistence(timeout: 10))
        app.buttons["issue-menu"].tap()

        let resync = app.buttons["Rebuild this card"]
        XCTAssertTrue(resync.waitForExistence(timeout: 3), "no rebuild entry")
        resync.tap()

        // The CARD comes back, not merely the page: the hatch throws away the
        // mirror and everything in memory and lets the cold-open path rebuild
        // it, so what proves it worked is the card's own text being drawn a
        // second time by a document that has no memory of the first.
        //
        // It used to assert the page's loading line instead, which was all the
        // demo could ever show — the card store talks to a gateway and there
        // is none — until `-baybo-demo-card` started seeding one (2026-08-26).
        // A hang and a rebuild look identical from the loading line.
        XCTAssertTrue(
            app.staticTexts["the dial loop drops its subscription"].waitForExistence(
                timeout: BayboUITestCase.webviewTimeout),
            "the card never came back after the rebuild")
        // And the native chrome around it survived.
        XCTAssertTrue(app.buttons["issue-menu"].exists)
    }

    /// **A Waiting row leaves on the PRESS.** The suite asserted only that the
    /// four kinds appear; nothing asserted any of them goes, which is how the
    /// strip shipped with two answers that changed nothing on screen for a
    /// whole round trip — and, under the demo, for ever.
    func testAnsweringAWaitingRowRetiresItImmediately() {
        let app = openBoard()
        let approve = app.buttons["waiting-approve-41"]
        XCTAssertTrue(approve.waitForExistence(timeout: 5), "no approval row")
        approve.tap()
        XCTAssertFalse(
            app.buttons["waiting-approve-41"].waitForExistence(timeout: 4),
            "an answered approval must leave the strip on the press")

    }

    /// The strip goes away entirely once the last prompt is answered — a header
    /// reading "WAITING ON YOU 0" over an empty box would be the same bug
    /// wearing a different number.
    func testTheStripDisappearsOnceTheLastPromptIsAnswered() {
        let app = openBoard()
        XCTAssertTrue(app.buttons["waiting-approve-41"].waitForExistence(timeout: 5))
        app.buttons["waiting-approve-41"].tap()
        XCTAssertFalse(
            app.otherElements["waiting-strip"].waitForExistence(timeout: 4),
            "an empty strip must not linger as a header over nothing")
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
