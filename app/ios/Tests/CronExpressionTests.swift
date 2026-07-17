import Foundation
import Testing

@testable import Baybo

/// The cron humanizer. Two properties matter more than any individual sentence:
/// it never describes an expression it does not fully understand, and it agrees
/// with the SCHEDULER about what a day number means.
@Suite @MainActor
struct CronExpressionTests {
    /// `DateFormatter` puts a NARROW NO-BREAK SPACE (U+202F) before AM/PM, so a
    /// plain-space literal never matches what the user sees.
    private static func plain(_ s: String) -> String {
        s.replacingOccurrences(of: "\u{202F}", with: " ")
            .replacingOccurrences(of: "\u{00A0}", with: " ")
    }

    private func en(_ expr: String) -> String {
        Self.plain(CronExpression.humanize(expr, lproj: "en"))
    }

    // MARK: - The shapes it can say

    @Test func aDailyExpressionReadsAsATimeOfDay() {
        #expect(en("0 9 * * *") == "Daily at 9:00 AM")
        #expect(en("30 17 * * *") == "Daily at 5:30 PM")
        // Midnight is a time like any other — not an absence of one.
        #expect(en("0 0 * * *") == "Daily at 12:00 AM")
    }

    /// **The trap.** Our scheduler is Quartz-numbered — 1=Sunday — so `1-5` is
    /// Sunday..Thursday, NOT the weekdays its author probably meant. Saying so
    /// is the entire point: the list reports what the gateway will do.
    /// (`days_of_week_are_quartz_numbered_from_sunday` pins the other side.)
    @Test func daysOfWeekFollowTheSchedulersQuartzNumbering() {
        #expect(en("0 9 * * 1") == "Sun at 9:00 AM", "1 is Sunday, not Monday")
        #expect(en("0 9 * * 6") == "Fri at 9:00 AM")
        #expect(en("0 9 * * 7") == "Sat at 9:00 AM")
        // A Unix-habit `1-5` means Sunday..Thursday here, and must read that way
        // rather than quietly claiming "weekdays".
        #expect(en("0 9 * * 1-5") == "Sun, Mon, Tue, Wed, and Thu at 9:00 AM")
    }

    @Test func weekdayNamesMatchTheirNumbers() {
        #expect(en("0 18 * * FRI") == en("0 18 * * 6"))
        #expect(en("0 18 * * fri") == en("0 18 * * 6"), "the field is case-insensitive")
        #expect(en("0 18 * * Sunday") == en("0 18 * * 1"))
    }

    /// The list itself is `ListFormatter`'s, so it follows the language's own
    /// conventions — including dropping the Oxford comma at two items.
    @Test func aDayListReadsAsAList() {
        #expect(en("0 9 * * 2,4,6") == "Mon, Wed, and Fri at 9:00 AM")
        // Out of order and duplicated: one sane list, not a transcript.
        #expect(en("0 9 * * 6,2,2") == "Mon and Fri at 9:00 AM")
    }

    @Test func stepsAndHoursReadAsIntervals() {
        #expect(en("*/30 * * * *") == "Every 30 minutes")
        #expect(en("0 * * * *") == "Hourly")
        #expect(en("15 * * * *") == "Hourly at :15")
        #expect(en("* * * * *") == "Every minute")
        #expect(en("0 */6 * * *") == "Every 6 hours")
    }

    @Test func aMonthlyExpressionNamesTheDay() {
        #expect(en("0 9 1 * *") == "Day 1 monthly at 9:00 AM")
    }

    /// The gateway normalizes a 5-field expression by prepending second 0, so a
    /// 6-field expression means the same schedule and must read the same.
    @Test func sixFieldExpressionsAreTheSameSchedule() {
        #expect(en("0 0 9 * * *") == en("0 9 * * *"))
        #expect(en("0 0 9 * * 6") == en("0 9 * * 6"))
    }

    // MARK: - The far more important half: what it refuses to say

    /// Anything outside the shapes above comes back verbatim. A row showing a
    /// cron expression is honest; a row confidently describing the wrong
    /// schedule is not, and this is a screen people will trust.
    @Test func anythingItCannotDescribeExactlyStaysRaw() {
        for expr in [
            "0 9 * 3 *",  // only in March — the sentence has no month
            "0 9 1 * 6",  // dom AND dow: cron ORs them, which no sentence conveys
            "30 9-17 * * *",  // an hour RANGE, not one time
            "0 9,17 * * *",  // two times of day
            "5/15 * * * *",  // a step from an offset, not `*/n`
            "0 9 * * 1#2",  // second Sunday — an operator we do not parse
            "0 9 * * 6L",  // last Friday
            "0 0 12 * * 1 2027",  // 7 fields: a year we never mention
            "0 9 * * 8",  // out of range — the scheduler would reject it too
            "0 9 * * 0",  // Unix Sunday: NOT valid here (the gateway 400s it)
            "0 9 * * MOO",  // not a day
            "*/0 * * * *",  // a step of zero
            "",  // nothing at all
            "nonsense",
            "0 9 * *",  // four fields
        ] {
            #expect(
                CronExpression.humanize(expr, lproj: "en") == expr,
                "must stay raw rather than be guessed at: \(expr)")
        }
    }

    /// A sub-minute schedule is a real thing our scheduler runs, and none of the
    /// sentences mention seconds — so it stays raw rather than reading as a
    /// once-a-minute job.
    @Test func aSecondsScheduleIsNotFlattenedIntoTheMinute() {
        #expect(CronExpression.humanize("*/10 * * * * *", lproj: "en") == "*/10 * * * * *")
        #expect(CronExpression.humanize("30 0 9 * * *", lproj: "en") == "30 0 9 * * *")
    }

    /// Whitespace is a formatting accident, not a different schedule.
    @Test func extraWhitespaceIsTolerated() {
        #expect(en("  0   9   *   *   * ") == "Daily at 9:00 AM")
    }

    // MARK: - Language

    /// The sentence is built from localized parts — the day names and the clock
    /// come from the OS, so they follow the app's language, not the phone's.
    @Test func theSentenceFollowsTheAppLanguage() {
        let zh = CronExpression.humanize("0 9 * * 6", lproj: "zh-Hans")
        #expect(zh.contains("周五"), "got \(zh)")
        #expect(!zh.contains("Fri"), "got \(zh)")

        let daily = CronExpression.humanize("0 9 * * *", lproj: "zh-Hans")
        #expect(daily.contains("每天"), "got \(daily)")
    }
}
