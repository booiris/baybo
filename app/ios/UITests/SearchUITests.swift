import XCTest

/// Headless drive of the search surface: the tab-bar search button, the
/// minimum-length gate, a result card, and the round trip that makes backing out
/// of a hit return to the results rather than to the chat list.
///
/// Runs against `-baybo-open-home` demo rows with `-baybo-demo-search` supplying
/// canned hits — there is no gateway in that mode, so without the stub every
/// state on this screen would be the failure page.
final class SearchUITests: BayboUITestCase {
    private func launchSearch() -> XCUIApplication {
        launch(["-baybo-open-home", "-baybo-demo-search"])
    }

    /// The `.search` role tab. iOS 26 renders it as a circle detached from the
    /// tab pill, but it is still an ordinary tab item to the accessibility tree.
    private func searchTab(_ app: XCUIApplication) -> XCUIElement {
        app.tabBars.buttons["Search"].firstMatch
    }

    private func openSearch(_ app: XCUIApplication) -> XCUIElement {
        let tab = searchTab(app)
        XCTAssertTrue(tab.waitForExistence(timeout: 10), "the tab bar must offer search")
        tab.tap()
        let field = app.textFields["search.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 5), "the search tab shows the field")
        return field
    }

    func testTheTabBarOffersSearchAndItShowsTheField() {
        let app = launchSearch()
        let field = openSearch(app)
        XCTAssertTrue(field.isHittable, "the field must be reachable, not merely laid out")
        // Idle draws nothing but the field — the placeholder is the only
        // instruction, so there is no hint element to assert on.
        XCTAssertFalse(app.buttons["search.hit.demo-1.2"].exists)
    }

    /// One character is a legitimate query the index can answer and a useless one
    /// to answer — it matches nearly every conversation — so nothing is searched.
    ///
    /// Asserted on the RESULTS, not on a hint: the idle screen draws no text of
    /// its own, and `-baybo-demo-search` answers any query of 2+ characters, so
    /// "no cards" is exactly "no search ran".
    func testASingleCharacterDoesNotSearch() {
        let app = launchSearch()
        let field = openSearch(app)
        field.tap()
        field.typeText("d")

        XCTAssertFalse(
            app.buttons["search.hit.demo-1.2"].waitForExistence(timeout: 2),
            "one character must not run a search")
    }

    func testTypingShowsGroupedResults() {
        let app = launchSearch()
        let field = openSearch(app)
        field.tap()
        field.typeText("demo")

        // Two conversations, and the first carries two separately tappable
        // excerpts (each hit is its own jump destination).
        XCTAssertTrue(
            app.buttons["search.hit.demo-1.2"].waitForExistence(timeout: 10),
            "the first conversation's best hit must be tappable on its own")
        XCTAssertTrue(app.buttons["search.hit.demo-1.3"].exists)
        XCTAssertTrue(app.buttons["search.hit.demo-2.1"].exists)
    }

    /// Opening a hit pushes onto the OUTER NavigationStack, which wraps the whole
    /// TabView — so the conversation covers the shell and this tab stays selected
    /// underneath it. `openSearchResult` passes `keepTab: true` for exactly this;
    /// without it `activateSession` would force `homeTab = .chats` and the back
    /// gesture would land on the chat list with the results gone.
    /// ✕ leaves search and restores the native tab bar. It returns to the tab
    /// the user came FROM (`tabBeforeSearch`), not a hardcoded Chats — entering
    /// search from Deck and being dumped on Chats is a different bug wearing the
    /// same clothes.
    func testTheExitButtonRestoresTheTabTheUserCameFrom() {
        let app = launchSearch()

        // Come in from Deck, not the default Chats.
        app.tabBars.buttons["Deck"].firstMatch.tap()
        XCTAssertTrue(app.tabBars.buttons["Deck"].firstMatch.isSelected)

        _ = openSearch(app)
        // The bar is gone while searching — the field stands in its place.
        // `isHittable`, not `exists`: a hidden bar can linger in the
        // accessibility tree, and what would actually be a bug is it still
        // taking taps from under the field.
        XCTAssertFalse(
            app.tabBars.buttons["Deck"].firstMatch.isHittable,
            "the hidden tab bar must not still be tappable under the search field")

        let exit = app.buttons["search.exit"]
        XCTAssertTrue(exit.waitForExistence(timeout: 5))
        exit.tap()

        XCTAssertTrue(
            app.tabBars.buttons["Deck"].firstMatch.waitForExistence(timeout: 5),
            "the native tab bar must come back")
        XCTAssertTrue(
            app.tabBars.buttons["Deck"].firstMatch.isSelected,
            "and land on the tab search was opened from")
    }

    func testOpeningAHitAndGoingBackReturnsToTheResults() {
        let app = launchSearch()
        let field = openSearch(app)
        field.tap()
        field.typeText("demo")

        let hit = app.buttons["search.hit.demo-1.2"]
        XCTAssertTrue(hit.waitForExistence(timeout: 10))
        hit.tap()

        // In the conversation: the composer is the unambiguous marker.
        let back = app.buttons["Back to conversations"]
        XCTAssertTrue(
            back.waitForExistence(timeout: Self.webviewTimeout),
            "tapping a hit must open the conversation")
        back.tap()

        XCTAssertTrue(
            app.textFields["search.field"].waitForExistence(timeout: 10),
            "backing out of the conversation must land on the RESULTS, not the chat list")
        XCTAssertTrue(
            app.buttons["search.hit.demo-1.2"].exists,
            "the query and its results must survive the round trip")
    }
}
