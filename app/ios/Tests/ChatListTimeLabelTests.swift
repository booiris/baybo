import Foundation
import Testing

@testable import Baybo

@Suite @MainActor
struct ChatListTimeLabelTests {
    private var calendar: Calendar {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(secondsFromGMT: 0) ?? .gmt
        return calendar
    }

    private func date(_ year: Int, _ month: Int, _ day: Int, _ hour: Int = 12) -> Date {
        calendar.date(from: DateComponents(year: year, month: month, day: day, hour: hour))
            ?? .distantPast
    }

    private func plain(_ text: String) -> String {
        text.replacingOccurrences(of: "\u{202F}", with: " ")
            .replacingOccurrences(of: "\u{00A0}", with: " ")
    }

    @Test func todayUsesATimeAndClampsFutureServerClocks() {
        let now = date(2026, 8, 29, 12)
        #expect(
            plain(
                ChatListTimeLabel.text(
                    date(2026, 8, 29, 9), locale: "en", relativeTo: now, calendar: calendar))
                == "9:00 AM")
        #expect(
            plain(
                ChatListTimeLabel.text(
                    date(2026, 8, 30), locale: "en", relativeTo: now, calendar: calendar))
                == "12:00 PM")
    }

    @Test func recentAndOlderRowsKeepTheirExistingGranularity() {
        let now = date(2026, 8, 29)
        #expect(
            ChatListTimeLabel.text(
                date(2026, 8, 25), locale: "en", relativeTo: now, calendar: calendar)
                == "Tue")
        #expect(
            ChatListTimeLabel.text(
                date(2026, 8, 1), locale: "en", relativeTo: now, calendar: calendar)
                == "8/1")
        #expect(
            ChatListTimeLabel.text(
                date(2025, 8, 1), locale: "en", relativeTo: now, calendar: calendar)
                == "8/1/2025")
    }
}
