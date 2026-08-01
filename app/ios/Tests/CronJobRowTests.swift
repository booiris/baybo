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
        expr: String = "0 9 * * *",
        timezone: String = "Asia/Shanghai",
        status: CronJobStatus = .enabled,
        nextTriggerAt: String? = nil
    ) -> CronJobSummary {
        CronJobSummary(
            id: "j1",
            title: title,
            prompt: prompt,
            expr: expr,
            timezone: timezone,
            status: status,
            nextTriggerAt: nextTriggerAt,
            lastTriggeredAt: nil,
            builtin: false)
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

    /// The row says the schedule in words and keeps the zone beside it —
    /// `CronExpression` owns the sentence (and its own tests); this pins that the
    /// row actually asks it, rather than printing the raw cron.
    @Test func aRecurringJobShowsItsScheduleInWordsWithTheZone() {
        #expect(
            Self.plain(CronJobRowView.scheduleLabel(job(), locale: "en"))
                == "Daily at 9:00 AM · Asia/Shanghai")
    }

    /// An expression the humanizer will not describe still has to render — as
    /// itself, never as a guess.
    @Test func anUndescribableExpressionFallsBackToTheRawCron() {
        #expect(
            CronJobRowView.scheduleLabel(job(expr: "0 9 * 3 *"), locale: "en")
                == "0 9 * 3 * · Asia/Shanghai")
    }

    /// `DateFormatter` separates the time from AM/PM with a NARROW NO-BREAK SPACE
    /// (U+202F), not a space — so a plain-space literal never matches what the
    /// user actually sees. Normalize rather than paste an invisible character
    /// into the expectation.
    private static func plain(_ s: String) -> String {
        s.replacingOccurrences(of: "\u{202F}", with: " ")
            .replacingOccurrences(of: "\u{00A0}", with: " ")
    }

    /// The gateway stamps some instants with fractional seconds and some without;
    /// a formatter configured for one silently rejects the other.
    @Test func bothRfc3339FlavoursParse() {
        #expect(CronJobRowView.parse("2026-07-20T10:00:00Z") != nil)
        #expect(CronJobRowView.parse("2026-07-20T10:00:00.123Z") != nil)
        #expect(CronJobRowView.parse("not a timestamp") == nil)
    }

    /// A job with no next trigger says WHY rather than showing a blank where a
    /// time goes. `.executed` cannot reach this list (one-shots are filtered in
    /// `list_cron_jobs`) but is the wire's third status, so it must render
    /// truthfully — and distinctly from paused — if it ever does.
    @Test func aJobWithNothingComingSaysWhyRatherThanShowingATime() {
        #expect(!CronJobRowView.nextLabel(job(status: .disabled), locale: "en").isEmpty)
        #expect(!CronJobRowView.nextLabel(job(status: .executed), locale: "en").isEmpty)
        #expect(
            CronJobRowView.nextLabel(job(status: .disabled), locale: "en")
                != CronJobRowView.nextLabel(job(status: .executed), locale: "en"),
            "paused and done must not read the same")
    }

    /// An enabled job with no next trigger renders nothing rather than a wrong or
    /// stale time. This is the live window right after a RESUME: the new trigger
    /// is the scheduler's to compute, so until the refetch lands the phone
    /// genuinely does not know when this next runs.
    @Test func anEnabledJobWithoutATriggerShowsNothing() {
        #expect(CronJobRowView.nextLabel(job(status: .enabled), locale: "en").isEmpty)
        #expect(
            CronJobRowView.nextLabel(
                job(status: .enabled, nextTriggerAt: "not a timestamp"), locale: "en"
            ).isEmpty)
    }

    @Test func anEnabledJobCountsDownToItsNextRun() {
        let soon = ISO8601DateFormatter().string(from: Date().addingTimeInterval(3 * 3600))
        let label = CronJobRowView.nextLabel(
            job(status: .enabled, nextTriggerAt: soon), locale: "en")
        #expect(!label.isEmpty)
        #expect(label.contains("2") || label.contains("3"), "got \(label)")
    }
}
