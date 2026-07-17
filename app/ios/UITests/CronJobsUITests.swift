import XCTest

/// Headless drive of the ☰ menu's scheduled-jobs list: the entry, the three row
/// states, and the tap into a job's history.
///
/// Runs against `-baybo-open-home`, where `AppStore.loadCronJobs` serves a local
/// fixture (`demoCronJobs`) — no gateway. What that leaves genuinely under test
/// up here is what only a running app has: that the menu entry exists and pushes,
/// that the screen resolves its rows out of the async load (a `nil`-vs-`[]`
/// mix-up would show "no scheduled jobs" forever), that the prompt fallback
/// reaches a titleless row, and that a tap crosses into the cron group.
final class CronJobsUITests: BayboUITestCase {
    private static let liveJob = "Morning brief"
    private static let pausedJob = "Weekly digest"
    /// The titleless fixture row: it must render its PROMPT, never blank.
    private static let namelessJob = "Remind me to renew the TLS certificate"
    private static let fireTitle = "Morning brief · 7/14"

    private func openCronJobs() -> XCUIApplication {
        let app = launch(["-baybo-open-home"])
        XCTAssertTrue(app.buttons["Menu"].waitForExistence(timeout: 5), "list never appeared")
        app.buttons["Menu"].tap()
        let entry = app.buttons["Scheduled jobs"]
        XCTAssertTrue(entry.waitForExistence(timeout: 3), "the menu offers no scheduled jobs entry")
        entry.tap()
        return app
    }

    /// Every live job, whatever shape: recurring and one-shot, enabled and not.
    /// The status labels are the only thing separating a job that will run from
    /// one that never will again, in a list that offers no way to ask.
    func testMenuOpensTheJobListShowingEveryLiveJobAndItsState() throws {
        let app = openCronJobs()

        XCTAssertTrue(
            app.staticTexts[Self.liveJob].waitForExistence(timeout: 5),
            "the job list never resolved its rows")
        XCTAssertTrue(app.staticTexts[Self.pausedJob].exists, "the paused job is missing")
        XCTAssertTrue(
            app.staticTexts[Self.namelessJob].exists,
            "a titleless job must fall back to its prompt, not render blank")

        // Its schedule, with the timezone — `0 9 * * *` means nothing without it.
        XCTAssertTrue(
            app.staticTexts["0 9 * * * · Asia/Shanghai"].exists,
            "the row does not show the schedule and its timezone")

        XCTAssertTrue(app.staticTexts["Paused"].exists, "a disabled job must read as paused")
        XCTAssertTrue(app.staticTexts["Done"].exists, "a spent one-shot must read as done")
    }

    /// The list is the only place a job appears as a SCHEDULE; tapping it crosses
    /// to the fires it has produced. That hop is the screen's one action.
    func testTappingAJobOpensItsExecutionRecords() throws {
        let app = openCronJobs()
        let job = app.staticTexts[Self.liveJob]
        XCTAssertTrue(job.waitForExistence(timeout: 5))
        job.tap()

        XCTAssertTrue(
            app.staticTexts[Self.fireTitle].waitForExistence(timeout: 3),
            "tapping a job did not open its fires")

        // Back retraces: fires → job list → chat list. Nothing is skipped.
        app.buttons["Back to conversations"].firstMatch.tap()
        XCTAssertTrue(
            app.staticTexts[Self.pausedJob].waitForExistence(timeout: 3),
            "back from the fires did not land on the job list")
        app.buttons["Back to conversations"].firstMatch.tap()
        XCTAssertTrue(
            app.buttons["Menu"].waitForExistence(timeout: 3),
            "back from the job list did not reach the chat list")
    }
}
