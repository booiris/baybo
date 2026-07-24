import Foundation

/// One row of the chat header's message index: a message the user sent, plus
/// the agent's answer to it as a one-line gloss.
///
/// Decoded verbatim from the transcript webview's `{type:"outline"}` post (the
/// `OutlineEntry` in `web/src/types.ts`). The web side owns this list and this
/// side never re-derives it — `ChatStore.send` seeds the optimistic bubble over
/// the bridge and never routes through `pushFrame`, so anything derived from
/// the native frame stream would not see this device's own message until the
/// server echoed it back, and an offline send would never appear at all.
///
/// Every field but `id` decodes tolerantly: the payload is hand-built JSON on
/// the far side of a seam whose marshalling fails silently, and one missing key
/// must not blank the sheet. `id` is required — an entry without one is not
/// jumpable, so it is a contract break worth surfacing rather than rendering.
struct OutlineEntry: Decodable, Identifiable, Equatable {
    /// The transcript row id, matched against the `data-row-id` the web side
    /// puts on every user message group. Never an ordinal — the rows this
    /// device sent carry a `platform_msg_id` and have no ordinal at all.
    let id: String
    /// The prompt, already whitespace-collapsed and capped web-side. Empty for
    /// an attachment-only send.
    let text: String
    /// The agent's answer, markdown flattened to one line. Empty while the turn
    /// is still running, when it was stopped, and when it produced no prose.
    let gloss: String
    /// Pre-formatted by the web side's `formatTimestampShort` — the same call
    /// that stamps the bubble's own clock, so the list and the transcript can
    /// never disagree and this side parses no dates.
    let at: String
    /// `YYYY-MM-DD` in device-local time. The one field this side formats.
    let dayKey: String
    let attachments: Int
    /// Set only while the send is unconfirmed.
    let state: SendState?

    enum SendState: String, Decodable {
        case sending
        case failed
    }

    private enum CodingKeys: String, CodingKey {
        case id, text, gloss, at, dayKey, attachments, state
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        text = (try? container.decode(String.self, forKey: .text)) ?? ""
        gloss = (try? container.decode(String.self, forKey: .gloss)) ?? ""
        at = (try? container.decode(String.self, forKey: .at)) ?? ""
        dayKey = (try? container.decode(String.self, forKey: .dayKey)) ?? ""
        attachments = (try? container.decode(Int.self, forKey: .attachments)) ?? 0
        state = try? container.decode(SendState.self, forKey: .state)
    }

    #if DEBUG
        init(
            id: String, text: String, gloss: String = "", at: String = "", dayKey: String = "",
            attachments: Int = 0, state: SendState? = nil
        ) {
            self.id = id
            self.text = text
            self.gloss = gloss
            self.at = at
            self.dayKey = dayKey
            self.attachments = attachments
            self.state = state
        }
    #endif
}

/// Pure grouping and labelling for the message-index sheet — no bridge, no FFI,
/// no filesystem, so it is unit-testable on its own (the `ApprovalQueue` shape).
enum MessageOutline {
    /// A run of entries sharing a day, in the thread's own order.
    struct Bucket: Identifiable, Equatable {
        let id: String
        let dayKey: String
        let entries: [OutlineEntry]
    }

    /// Split into day runs. Deliberately not a dictionary group: the sheet is a
    /// timeline, so the order is the transcript's, and entries with no
    /// timestamp (`dayKey` empty — a send that has not been stamped yet) form
    /// their own unlabelled run rather than being hoisted into some other day.
    static func buckets(_ entries: [OutlineEntry]) -> [Bucket] {
        var out: [Bucket] = []
        var run: [OutlineEntry] = []
        var runKey: String?
        for entry in entries {
            if entry.dayKey != runKey, !run.isEmpty, let key = runKey {
                out.append(Bucket(id: "\(out.count)-\(key)", dayKey: key, entries: run))
                run = []
            }
            runKey = entry.dayKey
            run.append(entry)
        }
        if let key = runKey, !run.isEmpty {
            out.append(Bucket(id: "\(out.count)-\(key)", dayKey: key, entries: run))
        }
        return out
    }

    /// The day header's label: today / yesterday, the weekday name within the
    /// last week, then a calendar date (gaining the year once it rolls over) —
    /// the chat list's time-column grammar, one granularity coarser.
    ///
    /// Both the words and the `Locale` come from `lproj`, never the ambient
    /// setting: a sentence whose template and OS-formatted parts speak
    /// different languages is exactly the "每Sun 9:00 AM" bug.
    static func dayLabel(_ dayKey: String, now: Date = Date(), lproj: String) -> String {
        guard let date = day(dayKey) else { return "" }
        let calendar = Calendar.current
        if calendar.isDateInToday(date) { return t("chat.indexToday", lproj) }
        if calendar.isDateInYesterday(date) { return t("chat.indexYesterday", lproj) }
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: lproj)
        let days = calendar.dateComponents(
            [.day], from: calendar.startOfDay(for: date), to: calendar.startOfDay(for: now)
        ).day
        if let days, Self.weekdayWindow.contains(days) {
            formatter.setLocalizedDateFormatFromTemplate("EEEE")
            return formatter.string(from: date)
        }
        let sameYear = calendar.isDate(date, equalTo: now, toGranularity: .year)
        formatter.setLocalizedDateFormatFromTemplate(sameYear ? "MMMd" : "yMMMd")
        return formatter.string(from: date)
    }

    /// Yesterday has its own word, and today is `isDateInToday`, so the weekday
    /// window opens at two days back and closes before the name would wrap
    /// around to an ambiguous one.
    private static let weekdayWindow = 2...6

    /// `dayKey` was computed from the device's own clock web-side, so it is read
    /// back in the device's zone. A fixed POSIX locale: the key is a wire
    /// format, not a display string, and a non-Gregorian device calendar must
    /// not reinterpret it.
    private static func day(_ dayKey: String) -> Date? {
        guard !dayKey.isEmpty else { return nil }
        let parser = DateFormatter()
        parser.locale = Locale(identifier: "en_US_POSIX")
        parser.calendar = Calendar(identifier: .gregorian)
        parser.timeZone = .current
        parser.dateFormat = "yyyy-MM-dd"
        return parser.date(from: dayKey)
    }

    private static func t(_ key: String, _ lproj: String) -> String {
        Lang.catalog(lproj: lproj).localizedString(forKey: key, value: key, table: nil)
    }
}
