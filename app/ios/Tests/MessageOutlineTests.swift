import Foundation
import Testing

@testable import Baybo

/// The message index's pure half: how the sheet splits a thread into day runs,
/// what it writes above each one, and how tolerantly an entry decodes off the
/// bridge. All three are invisible in a screenshot of the common case — a
/// single-day thread in the tester's own language — and wrong only on the days
/// nobody is looking.
@Suite @MainActor
struct MessageOutlineTests {
    private static func entry(_ id: String, dayKey: String) -> OutlineEntry {
        OutlineEntry(id: id, text: "prompt \(id)", dayKey: dayKey)
    }

    private static func ids(_ bucket: MessageOutline.Bucket) -> [String] {
        bucket.entries.map(\.id)
    }

    /// The `dayKey` a date carries — the same wire format the web side writes
    /// (device-local, Gregorian), so a test never depends on the machine's
    /// locale or calendar.
    private static func dayKey(_ date: Date) -> String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.timeZone = .current
        formatter.dateFormat = "yyyy-MM-dd"
        return formatter.string(from: date)
    }

    private static func date(_ year: Int, _ month: Int, _ day: Int) -> Date? {
        Calendar.current.date(from: DateComponents(year: year, month: month, day: day, hour: 12))
    }

    private static func decode(_ json: String) throws -> OutlineEntry {
        try JSONDecoder().decode(OutlineEntry.self, from: Data(json.utf8))
    }

    @Test func anEmptyIndexHasNoBuckets() {
        #expect(MessageOutline.buckets([]).isEmpty)
    }

    @Test func consecutiveEntriesOfOneDayShareABucket() {
        let buckets = MessageOutline.buckets([
            Self.entry("a", dayKey: "2026-07-22"),
            Self.entry("b", dayKey: "2026-07-22"),
            Self.entry("c", dayKey: "2026-07-23"),
        ])
        #expect(buckets.count == 2)
        #expect(buckets.map(\.dayKey) == ["2026-07-22", "2026-07-23"])
        #expect(Self.ids(buckets[0]) == ["a", "b"])
        #expect(Self.ids(buckets[1]) == ["c"])
    }

    /// A day that comes back later in the thread is a SECOND run, not a merge
    /// into the first: the sheet is a timeline in the transcript's own order, and
    /// hoisting a later entry up to its earlier day would make a row's tap land
    /// somewhere the reader did not point at. (Real threads can't interleave
    /// days, but a clock change or a rebased ordinal can hand us one.)
    @Test func aDayThatRecursOpensItsOwnRun() {
        let buckets = MessageOutline.buckets([
            Self.entry("a", dayKey: "2026-07-22"),
            Self.entry("b", dayKey: "2026-07-23"),
            Self.entry("c", dayKey: "2026-07-22"),
        ])
        #expect(buckets.map(\.dayKey) == ["2026-07-22", "2026-07-23", "2026-07-22"])
        #expect(buckets.map(\.id).count == Set(buckets.map(\.id)).count, "List needs unique ids")
    }

    /// A send with no timestamp yet (in flight, or failed before the server
    /// stamped it) has an empty `dayKey`, and those form their own run rather
    /// than joining whichever day happened to precede them.
    @Test func unstampedEntriesRunSeparately() {
        let buckets = MessageOutline.buckets([
            Self.entry("a", dayKey: "2026-07-23"),
            Self.entry("b", dayKey: ""),
            Self.entry("c", dayKey: ""),
        ])
        #expect(buckets.count == 2)
        #expect(buckets[1].dayKey.isEmpty)
        #expect(Self.ids(buckets[1]) == ["b", "c"])
    }

    @Test func anUnstampedRunGetsNoHeader() {
        #expect(MessageOutline.dayLabel("", now: Date(), lproj: "en").isEmpty)
        #expect(MessageOutline.dayLabel("last tuesday", now: Date(), lproj: "en").isEmpty)
    }

    /// `isDateInToday`/`isDateInYesterday` are the one thing here that reads the
    /// machine's clock, so these two days are sampled from a single `now` rather
    /// than written down — and every other case below uses a date far enough in
    /// the past that the ambient day cannot reach it.
    @Test func todayAndYesterdayHaveTheirOwnWordInBothLanguages() throws {
        let now = Date()
        let yesterday = try #require(Calendar.current.date(byAdding: .day, value: -1, to: now))
        #expect(MessageOutline.dayLabel(Self.dayKey(now), now: now, lproj: "en") == "Today")
        #expect(MessageOutline.dayLabel(Self.dayKey(now), now: now, lproj: "zh-Hans") == "今天")
        #expect(
            MessageOutline.dayLabel(Self.dayKey(yesterday), now: now, lproj: "en") == "Yesterday")
        #expect(
            MessageOutline.dayLabel(Self.dayKey(yesterday), now: now, lproj: "zh-Hans") == "昨天")
    }

    /// ONE day, labelled from three different `now`s: inside the week it is a
    /// weekday name, past it a calendar date, and once the year has rolled over
    /// that date gains its year. The injected `now` is what moves the answer —
    /// reading the ambient clock would make this pass or fail by the date it ran.
    @Test func theLabelCoarsensAsNowAdvances() throws {
        let friday = Self.dayKey(try #require(Self.date(2025, 3, 14)))
        let insideTheWeek = try #require(Self.date(2025, 3, 17))
        let sameYear = try #require(Self.date(2025, 4, 20))
        let nextYear = try #require(Self.date(2026, 4, 20))

        #expect(MessageOutline.dayLabel(friday, now: insideTheWeek, lproj: "en") == "Friday")
        #expect(MessageOutline.dayLabel(friday, now: insideTheWeek, lproj: "zh-Hans").contains("五"))

        for lproj in ["en", "zh-Hans"] {
            let dated = MessageOutline.dayLabel(friday, now: sameYear, lproj: lproj)
            #expect(dated.contains("14"), "got \(dated)")
            #expect(!dated.contains("2025"), "a date in this year drops the year, got \(dated)")
            #expect(MessageOutline.dayLabel(friday, now: nextYear, lproj: lproj).contains("2025"))
        }
    }

    /// The bridge hand-builds this payload across a seam that fails silently, so
    /// an entry carrying nothing but its jump handle still has to render.
    @Test func anEntryMissingEveryOptionalFieldDecodes() throws {
        let entry = try Self.decode(#"{"id":"m42"}"#)
        #expect(entry.id == "m42")
        #expect(entry.text.isEmpty)
        #expect(entry.gloss.isEmpty)
        #expect(entry.at.isEmpty)
        #expect(entry.dayKey.isEmpty)
        #expect(entry.attachments == 0)
        #expect(entry.state == nil)
    }

    /// A send state this build doesn't know (an older/newer web bundle) drops to
    /// "no state" instead of taking the whole row down with it.
    @Test func anUnknownSendStateLeavesTheRestOfTheEntry() throws {
        #expect(try Self.decode(#"{"id":"m1","state":"failed"}"#).state == .failed)
        let entry = try Self.decode(#"{"id":"m1","text":"hi","state":"queued"}"#)
        #expect(entry.state == nil)
        #expect(entry.text == "hi")
    }

    /// An entry with no id is not jumpable — the one field worth failing over,
    /// because rendering it would offer the reader a row that goes nowhere.
    @Test func anEntryWithoutAnIdIsRejected() {
        #expect(throws: (any Error).self) {
            try Self.decode(#"{"text":"prompt","dayKey":"2026-07-23"}"#)
        }
    }
}
