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

    /// The new-board form is a pushed route, not a sheet — deliberately, so the
    /// name field can rise with the keyboard (the home shell opts out of
    /// keyboard avoidance wholesale). Create stays disabled until it is named.
    func testTheNewBoardFormRefusesToCreateUntilItIsNamed() {
        let app = openProjects()
        let new = app.buttons["project-new"]
        XCTAssertTrue(new.waitForExistence(timeout: 5), "no new-board affordance")
        new.tap()

        let name = app.textFields["new-project-name"]
        XCTAssertTrue(name.waitForExistence(timeout: 5), "the new-board form never pushed")
        let create = app.buttons["new-project-create"]
        XCTAssertTrue(create.exists)
        XCTAssertFalse(create.isEnabled, "an unnamed board must not be creatable")

        name.tap()
        name.typeText("rglide-2")
        XCTAssertTrue(create.isEnabled, "a named board should be creatable")
    }
}
