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

        // The schedule reads as a sentence, with the timezone — `0 9 * * *` is
        // punctuation to most people, and means nothing without whose 9am.
        // Matched around the time rather than through it: the formatter puts a
        // NARROW NO-BREAK SPACE before AM, so an exact label match would need an
        // invisible character pasted into this file.
        XCTAssertTrue(
            app.staticTexts.matching(
                NSPredicate(
                    format: "label BEGINSWITH %@ AND label ENDSWITH %@",
                    "Daily at", "· Asia/Shanghai")
            ).firstMatch.exists,
            "the row does not say the schedule in words with its timezone")

        XCTAssertTrue(app.staticTexts["Paused"].exists, "a disabled job must read as paused")
        XCTAssertTrue(app.staticTexts["Done"].exists, "a spent one-shot must read as done")
    }

    /// Pause and resume are one toggle, so the swipe must offer the verb the row
    /// is NOT already in — the demo's mutations resolve locally, so what is under
    /// test is the flip reaching the row and the swipe re-reading it.
    func testPauseAndResumeFlipTheRow() throws {
        let app = openCronJobs()
        let live = app.staticTexts[Self.liveJob]
        XCTAssertTrue(live.waitForExistence(timeout: 5))
        // COUNTED, not merely present: the fixture already contains one paused
        // job, so "is there a Paused label" is true before this test does
        // anything and would pass a resume that did nothing.
        let pausedLabels = app.staticTexts.matching(identifier: "Paused")
        XCTAssertEqual(pausedLabels.count, 1, "the fixture's own paused job")

        live.swipeLeft()
        let pause = app.buttons["Pause"]
        XCTAssertTrue(pause.waitForExistence(timeout: 3), "a live job offers no Pause")
        pause.tap()
        XCTAssertTrue(
            app.staticTexts.matching(identifier: "Paused").count == 2,
            "pausing did not mark the row paused")

        // Now paused, the same swipe must offer the other verb.
        live.swipeLeft()
        let resume = app.buttons["Resume"]
        XCTAssertTrue(resume.waitForExistence(timeout: 3), "a paused job offers no Resume")
        XCTAssertFalse(app.buttons["Pause"].exists, "a paused job must not offer Pause too")
        resume.tap()
        XCTAssertTrue(
            app.staticTexts.matching(identifier: "Paused").count == 1,
            "resuming did not clear the row's paused label")
    }

    /// A one-shot that has already run can only be deleted: there is nothing to
    /// pause, and resuming it is a 400 server-side (no future left in its
    /// schedule), so the row must not offer a verb that cannot work.
    func testASpentOneShotOffersOnlyDelete() throws {
        let app = openCronJobs()
        let spent = app.staticTexts[Self.namelessJob]
        XCTAssertTrue(spent.waitForExistence(timeout: 5))
        spent.swipeLeft()

        XCTAssertTrue(app.buttons["Delete"].waitForExistence(timeout: 3))
        XCTAssertFalse(app.buttons["Pause"].exists, "nothing to pause on a job that has run")
        XCTAssertFalse(app.buttons["Resume"].exists, "resuming a spent one-shot cannot work")
    }

    /// Delete names the job and states the blast radius that is the MIRROR of the
    /// group delete's: the schedule stops, the history stays.
    func testDeleteConfirmsByNameThenRemovesTheJob() throws {
        let app = openCronJobs()
        let doomed = app.staticTexts[Self.pausedJob]
        XCTAssertTrue(doomed.waitForExistence(timeout: 5))
        doomed.swipeLeft()

        let delete = app.buttons["Delete"]
        XCTAssertTrue(delete.waitForExistence(timeout: 3), "the job swipe revealed no Delete")
        delete.tap()

        XCTAssertTrue(
            app.staticTexts["Delete this scheduled job?"].waitForExistence(timeout: 3),
            "the job delete confirm did not present")
        XCTAssertTrue(
            app.staticTexts.matching(
                NSPredicate(format: "label CONTAINS %@", "“\(Self.pausedJob)”")
            ).firstMatch.exists,
            "the confirm does not name the job it will stop")
        XCTAssertTrue(
            app.staticTexts.matching(
                NSPredicate(format: "label CONTAINS[c] %@", "records it has already produced are kept")
            ).firstMatch.exists,
            "the confirm does not say the execution records survive")

        try dialogCommit(app, "Delete").tap()
        XCTAssertTrue(
            doomed.waitForNonExistence(timeout: 3), "the confirmed delete kept the job listed")
        XCTAssertTrue(app.staticTexts[Self.liveJob].exists, "it took an unrelated job with it")
    }

    /// A job that has never fired has no cron group to open — the group is a view
    /// over fires that do not exist. It must still say so in its own name rather
    /// than showing a blank page under a generic title: the jobs list is the only
    /// way to reach this state, and it just learned the name.
    func testAJobWithNoRunsOpensANamedEmptyHistory() throws {
        let app = openCronJobs()
        let job = app.staticTexts[Self.pausedJob]
        XCTAssertTrue(job.waitForExistence(timeout: 5))
        job.tap()

        XCTAssertTrue(
            app.staticTexts["No execution records yet"].waitForExistence(timeout: 3),
            "a fire-less job's history does not say it is empty")
        XCTAssertTrue(
            app.staticTexts[Self.pausedJob].exists,
            "the empty history is headed by the generic fallback, not the job's name")
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
