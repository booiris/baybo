import Foundation
import Testing

@testable import Baybo

/// The scheduled-jobs row's pure labelling. Every case here is something a
/// screenshot shows only on a device configured to expose it — which is exactly
/// why they are pinned below the UI.
@Suite @MainActor
struct CronJobRowTests {
    private func job(
        title: String = "Morning brief",
        prompt: String = "Summarize the news",
        schedule: CronScheduleSpec = .recurring(expr: "0 9 * * *"),
        timezone: String = "Asia/Shanghai",
        status: CronJobStatus = .enabled,
        nextTriggerAt: String? = nil
    ) -> CronJobSummary {
        CronJobSummary(
            id: "j1",
            title: title,
            prompt: prompt,
            schedule: schedule,
            timezone: timezone,
            status: status,
            nextTriggerAt: nextTriggerAt,
            lastTriggeredAt: nil)
    }

    @Test func aNamedJobShowsItsName() {
        #expect(CronJobRowView.headline(job()) == "Morning brief")
    }

    /// A row minted before `title` existed falls back to the prompt — the same
    /// choice the web table makes. Without it the row is an alarm glyph and a
    /// void, and the list's whole purpose is naming what is scheduled.
    @Test func aTitlelessJobFallsBackToItsPrompt() {
        #expect(CronJobRowView.headline(job(title: "")) == "Summarize the news")
    }

    @Test func aRecurringJobShowsItsExpressionAndZone() {
        #expect(
            CronJobRowView.scheduleLabel(job(), locale: "en_US")
                == "0 9 * * * · Asia/Shanghai")
    }

    /// **The one-shot's instant renders in the JOB's zone, not the device's.**
    ///
    /// One instant, two jobs that differ only in the zone they were authored in:
    /// each must show ITS OWN wall clock, so the label and the number agree. This
    /// shape is deliberate — asserting one job against one expected string would
    /// pass or fail on the HOST's timezone, and the bug it guards (`10:00Z`
    /// rendered as "6:00 PM · UTC") is invisible on a UTC machine, which is
    /// precisely what CI is.
    @Test func aOneShotRendersInItsOwnZoneNotTheDevices() {
        let instant = "2026-07-20T10:00:00Z"
        let utc = CronJobRowView.scheduleLabel(
            job(schedule: .once(time: instant), timezone: "UTC"), locale: "en_US")
        let shanghai = CronJobRowView.scheduleLabel(
            job(schedule: .once(time: instant), timezone: "Asia/Shanghai"), locale: "en_US")

        #expect(Self.plain(utc).contains("10:00 AM"), "got \(utc)")
        #expect(utc.hasSuffix("· UTC"))
        // The same moment, eight hours east.
        #expect(Self.plain(shanghai).contains("6:00 PM"), "got \(shanghai)")
        #expect(shanghai.hasSuffix("· Asia/Shanghai"))
    }

    /// `DateFormatter` separates the time from AM/PM with a NARROW NO-BREAK SPACE
    /// (U+202F), not a space — so a plain-space literal never matches what the
    /// user actually sees. Normalize rather than paste an invisible character
    /// into the expectation.
    private static func plain(_ s: String) -> String {
        s.replacingOccurrences(of: "\u{202F}", with: " ")
            .replacingOccurrences(of: "\u{00A0}", with: " ")
    }

    /// An unparseable zone must not silently render against the device's clock
    /// under a label naming another one. The raw RFC 3339 is ugly but true.
    @Test func anUnknownZoneFallsBackToTheRawInstant() {
        let label = CronJobRowView.scheduleLabel(
            job(schedule: .once(time: "2026-07-20T10:00:00Z"), timezone: "Mars/Olympus"),
            locale: "en_US")
        #expect(label == "2026-07-20T10:00:00Z · Mars/Olympus")
    }

    /// The gateway stamps some instants with fractional seconds and some without;
    /// a formatter configured for one silently rejects the other.
    @Test func bothRfc3339FlavoursParse() {
        #expect(CronJobRowView.parse("2026-07-20T10:00:00Z") != nil)
        #expect(CronJobRowView.parse("2026-07-20T10:00:00.123Z") != nil)
        #expect(CronJobRowView.parse("not a timestamp") == nil)
    }

    /// A paused job and a spent one-shot have no next trigger. In a read-only
    /// list the label is the ONLY thing saying they will not run — an empty
    /// column would render them identical to a live job.
    @Test func aJobWithNothingComingSaysWhyRatherThanShowingATime() {
        #expect(!CronJobRowView.nextLabel(job(status: .disabled), locale: "en_US").isEmpty)
        #expect(!CronJobRowView.nextLabel(job(status: .executed), locale: "en_US").isEmpty)
        #expect(
            CronJobRowView.nextLabel(job(status: .disabled), locale: "en_US")
                != CronJobRowView.nextLabel(job(status: .executed), locale: "en_US"),
            "paused and done must not read the same")
    }

    /// An enabled job with no next trigger (a gateway that omitted it) renders
    /// nothing rather than a wrong or stale time.
    @Test func anEnabledJobWithoutATriggerShowsNothing() {
        #expect(CronJobRowView.nextLabel(job(status: .enabled), locale: "en_US").isEmpty)
        #expect(
            CronJobRowView.nextLabel(
                job(status: .enabled, nextTriggerAt: "not a timestamp"), locale: "en_US"
            ).isEmpty)
    }

    @Test func anEnabledJobCountsDownToItsNextRun() {
        let soon = ISO8601DateFormatter().string(from: Date().addingTimeInterval(3 * 3600))
        let label = CronJobRowView.nextLabel(
            job(status: .enabled, nextTriggerAt: soon), locale: "en_US")
        #expect(!label.isEmpty)
        #expect(label.contains("2") || label.contains("3"), "got \(label)")
    }
}
