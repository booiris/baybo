import XCTest

/// Re-tapping the ALREADY-selected Chats tab steps the list down to the next row
/// carrying unread (`docs/chat-list.md`, `docs/navigation.md`).
///
/// The walk rule itself is pinned far more cheaply in `NextUnreadStepTests`. What
/// only this tier can answer is whether the gesture REACHES anything: SwiftUI ships
/// no tab-reselection API, so the whole feature hangs on `TabView` calling its
/// selection binding with an UNCHANGED value. Nothing in Swift's type system says
/// it does.
///
/// `-baybo-appstore-data` is what makes the list taller than the screen — the
/// six-row default fixture never scrolls, so a displacement assertion on it would
/// pass vacuously. Its unread stops, in rendered order: the cron GROUP `Morning
/// brief` (2), `Developer toolkit review` (1), and the bottom-most row (1).
final class UnreadStepUITests: BayboUITestCase {
    private static let homeFixture = ["-baybo-open-home", "-baybo-appstore-data"]
    /// The walk's first stop under this fixture, and a cron GROUP — so this also
    /// covers "a group is one stop" reaching the screen.
    private static let firstStopTitle = "Morning brief"
    /// The second stop: on screen at launch but low enough that walking onto it is
    /// an unambiguous move.
    private static let secondStopTitle = "Developer toolkit review"

    private func chatsTab(_ app: XCUIApplication) -> XCUIElement {
        app.tabBars.buttons["Chats"].firstMatch
    }

    private func listedHome() -> XCUIApplication {
        let app = launch(Self.homeFixture)
        XCTAssertTrue(
            app.staticTexts[Self.firstStopTitle].waitForExistence(timeout: 10),
            "the demo list never rendered")
        return app
    }

    func testRetappingTheSelectedChatsTabStepsToAnUnreadRow() {
        let app = listedHome()
        let stop = app.staticTexts[Self.secondStopTitle]
        let before = stop.frame.minY

        let chats = chatsTab(app)
        XCTAssertTrue(chats.isSelected, "the fixture should open on Chats")
        chats.tap()
        chats.tap()

        XCTAssertLessThan(
            stop.frame.minY, before - 100,
            "two re-taps should have walked the list down onto \(Self.secondStopTitle)")
    }

    /// The landing clearance is the list's top safe-area padding, and a row parked
    /// under the header veil would still report `exists` and `isHittable` — so the
    /// frame is the only meter that can see this. Under a `.contentMargins`
    /// clearance the row landed at 62pt, beneath a wordmark whose own frame ends at
    /// ~98pt; that is the regression this pins.
    func testTheSteppedRowLandsClearOfTheHeader() {
        let app = listedHome()
        let stop = app.staticTexts[Self.firstStopTitle]

        chatsTab(app).tap()

        XCTAssertGreaterThan(
            stop.frame.minY, app.staticTexts["Baybo"].frame.maxY,
            "the stepped row landed under the header")
    }

    /// Each tap moves ON. Nothing is marked read, so the sequence cannot advance by
    /// badges clearing — only by the cursor.
    func testEachTapMovesOn() {
        let app = listedHome()
        let stop = app.staticTexts[Self.firstStopTitle]

        let chats = chatsTab(app)
        chats.tap()
        let landed = stop.frame.minY
        chats.tap()

        XCTAssertLessThan(
            stop.frame.minY, landed - 40,
            "the second tap should have stepped past the group, not re-landed on it")
    }
}
