import XCTest

/// Headless drive of the search surface: the tab-bar search button, the
/// single-character query, a result card, and the round trip that makes backing out
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

    func testASingleCharacterSearches() {
        let app = launchSearch()
        let field = openSearch(app)
        field.tap()
        field.typeText("d")

        XCTAssertTrue(
            app.buttons["search.hit.demo-1.2"].waitForExistence(timeout: 10),
            "one character must run a search")
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
        // `exists`, NOT `isHittable`. This assertion used to check hittability
        // and passed against a bar that was never hidden at all: the keyboard
        // covers it, and a covered element is unhittable whether or not it is
        // hidden. `.toolbar(.hidden, for: .tabBar)` was on the TabView instead of
        // the tab's CONTENT, where it does nothing — the bar sat under the
        // keyboard with ~37pt protruding below it (keyboard ends at y=816, the
        // bar ran to y=853) and that strip was visible as a dark band. Only
        // absence from the tree distinguishes hidden from covered.
        XCTAssertFalse(
            app.tabBars.buttons["Deck"].firstMatch.exists,
            "the tab bar must be GONE while searching, not merely covered")

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
        // And NO keyboard. The field is first responder when the conversation is
        // pushed, and UIKit restores first responder on the pop — so without
        // dropping focus at the tap, coming back re-presents the keyboard over
        // the results (visibly: a grey snapshot, then the live keyboard).
        XCTAssertEqual(
            app.keyboards.count, 0,
            "coming back from a conversation must not raise the keyboard")
    }
}
